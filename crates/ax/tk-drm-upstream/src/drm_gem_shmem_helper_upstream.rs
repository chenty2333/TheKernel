// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_gem_shmem_helper.c` (Virtio V0).
//!
//! The object owns real LinuxKPI shmem pages. Page arrays hold page-cache
//! references; pin and vmap counts are separate; SG tables are built from the
//! pages and DMA-mapped only for the driver-facing `_pages_sgt` path.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_int, c_uint, c_void},
    ptr,
    sync::atomic::{AtomicI32, Ordering},
};

use tk_linux_kapi::{
    assert::warn_on,
    config::{
        EINVAL, EIO, EOPNOTSUPP, EOVERFLOW, GFP_KERNEL, IS_ERR, PAGE_SHIFT, PAGE_SIZE, PTR_ERR,
    },
    list::INIT_LIST_HEAD,
    memory::{
        kfree, kmalloc, kzalloc_obj, refcount_dec_and_test, refcount_inc_not_zero, refcount_read,
        refcount_set,
    },
    mm::{
        VM_DONTDUMP, VM_DONTEXPAND, VM_FAULT_FALLBACK, VM_FAULT_NOPAGE, VM_FAULT_SIGBUS, VM_PFNMAP,
        VmAreaStruct, VmFault, VmOperationsStruct,
    },
    scatterlist::{sg_alloc_table_from_pages_segment, sg_free_table},
    shmem::{
        folio_mark_accessed, folio_mark_dirty, fput_file, page_folio, page_to_phys,
        shmem_file_setup, shmem_read_mapping_page_gfp, shmem_truncate_range,
    },
    types::{Page, SgTable},
    vm::{PAGE_KERNEL, PAGE_KERNEL_WC, vmap, vunmap},
};

use crate::{
    dma_resv_types::DmaResv,
    drm_device_types::DrmDevice,
    drm_drv_types::DrmDriver,
    drm_file_types::DrmFile,
    drm_gem_shmem_helper_types_upstream::{
        DRM_GEM_SHMEM_MAP_WC, DRM_GEM_SHMEM_PAGES_MARK_ACCESSED_ON_PUT,
        DRM_GEM_SHMEM_PAGES_MARK_DIRTY_ON_PUT, DRM_GEM_SHMEM_PURGED, DrmGemShmemObject,
    },
    drm_gem_types::{DrmGemObject, DrmGemObjectFuncs},
    drm_modeset_lock_types::DrmModesetAcquireCtx,
    drm_prime_upstream::{DmaBuf, DmaBufAttachment},
    drm_print_types::DrmPrinter,
    iosys_map_types::{IosysMap, IosysMapAnon0},
    uapi_drm_mode_types::DrmModeCreateDumb,
};

const DMA_BIDIRECTIONAL: i32 = 0;
const SHMEM_PAGES_GFP: u32 =
    GFP_KERNEL | tk_linux_kapi::memory::__GFP_RETRY_MAYFAIL | tk_linux_kapi::config::__GFP_NOWARN;

unsafe extern "C" {
    fn dma_map_sgtable(dev: *mut c_void, sgt: *mut SgTable, direction: i32, attrs: u64) -> i32;
    fn dma_buf_attach(dmabuf: *mut DmaBuf, dev: *mut c_void) -> *mut DmaBufAttachment;
    fn dma_buf_detach(dmabuf: *mut DmaBuf, attach: *mut DmaBufAttachment);
    fn dma_buf_unmap_attachment_unlocked(
        attach: *mut DmaBufAttachment,
        sgt: *mut SgTable,
        direction: c_int,
    );
    fn dma_buf_put(dmabuf: *mut DmaBuf);
    fn drm_mode_size_dumb(
        dev: *mut DrmDevice,
        args: *mut DrmModeCreateDumb,
        hw_pitch_align: usize,
        hw_size_align: usize,
    ) -> i32;
}

#[inline]
unsafe fn shmem_is_imported(obj: *const DrmGemObject) -> bool {
    unsafe { !(*obj).import_attach.is_null() }
}

#[inline]
unsafe fn shmem_from_obj(obj: *mut DrmGemObject) -> *mut DrmGemShmemObject {
    obj.cast()
}

#[inline]
unsafe fn pages_count(shmem: *const DrmGemShmemObject) -> usize {
    unsafe { (*shmem).base.size >> PAGE_SHIFT }
}

#[inline]
unsafe fn shmem_flags(shmem: *const DrmGemShmemObject) -> u8 {
    unsafe { (*shmem).flags }
}

#[inline]
unsafe fn shmem_set_flags(shmem: *mut DrmGemShmemObject, flags: u8) {
    unsafe { (*shmem).flags = flags }
}

#[inline]
unsafe fn shmem_is_wc(shmem: *const DrmGemShmemObject) -> bool {
    unsafe { shmem_flags(shmem) & DRM_GEM_SHMEM_MAP_WC != 0 }
}

#[inline]
unsafe fn shmem_mapping(shmem: *mut DrmGemShmemObject) -> *mut tk_linux_kapi::shmem::AddressSpace {
    unsafe {
        if (*shmem).base.filp.is_null() {
            ptr::null_mut()
        } else {
            (*(*shmem).base.filp).f_mapping.cast()
        }
    }
}

#[inline]
unsafe fn dma_resv_lock(resv: *mut DmaResv, ctx: *mut DrmModesetAcquireCtx) -> i32 {
    #[cfg(test)]
    {
        let _ = (resv, ctx);
        0
    }
    #[cfg(not(test))]
    unsafe {
        tk_linux_kapi::ww_mutex::ww_mutex_lock(ptr::addr_of_mut!((*resv).lock), ctx.cast())
    }
}

#[inline]
unsafe fn dma_resv_lock_interruptible(resv: *mut DmaResv, ctx: *mut DrmModesetAcquireCtx) -> i32 {
    #[cfg(test)]
    {
        let _ = (resv, ctx);
        0
    }
    #[cfg(not(test))]
    unsafe {
        tk_linux_kapi::ww_mutex::ww_mutex_lock_interruptible(
            ptr::addr_of_mut!((*resv).lock).cast(),
            ctx.cast(),
        )
    }
}

#[inline]
unsafe fn dma_resv_unlock(resv: *mut DmaResv) {
    #[cfg(not(test))]
    unsafe {
        tk_linux_kapi::ww_mutex::ww_mutex_unlock(ptr::addr_of_mut!((*resv).lock));
    }
    #[cfg(test)]
    let _ = resv;
}

#[inline]
unsafe fn refcount_dec_not_one(value: *mut tk_linux_kapi::types::RefcountT) -> bool {
    unsafe {
        let counter = &*ptr::addr_of!((*value).refs.counter).cast::<AtomicI32>();
        let mut old = counter.load(Ordering::Acquire);
        loop {
            if old <= 1 {
                return false;
            }
            match counter.compare_exchange_weak(old, old - 1, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(observed) => old = observed,
            }
        }
    }
}

#[repr(transparent)]
struct SyncDrmGemObjectFuncs(DrmGemObjectFuncs);
unsafe impl Sync for SyncDrmGemObjectFuncs {}

static DRM_GEM_SHMEM_FUNCS: SyncDrmGemObjectFuncs = SyncDrmGemObjectFuncs(DrmGemObjectFuncs {
    free: Some(drm_gem_shmem_object_free),
    open: None,
    close: None,
    print_info: Some(drm_gem_shmem_object_print_info),
    export: None,
    pin: Some(drm_gem_shmem_object_pin),
    unpin: Some(drm_gem_shmem_object_unpin),
    get_sg_table: Some(drm_gem_shmem_object_get_sg_table),
    vmap: Some(drm_gem_shmem_object_vmap),
    vunmap: Some(drm_gem_shmem_object_vunmap),
    mmap: Some(drm_gem_shmem_object_mmap),
    evict: None,
    status: None,
    rss: None,
    vm_ops: &DRM_GEM_SHMEM_VM_OPS.0 as *const VmOperationsStruct as *mut c_void,
});

#[repr(transparent)]
struct SyncVmOperations(VmOperationsStruct);
unsafe impl Sync for SyncVmOperations {}

static DRM_GEM_SHMEM_VM_OPS: SyncVmOperations = SyncVmOperations(VmOperationsStruct {
    open: Some(drm_gem_shmem_vm_open),
    close: Some(drm_gem_shmem_vm_close),
    mapped: None,
    may_split: None,
    mremap: None,
    mprotect: None,
    fault: Some(drm_gem_shmem_fault),
    huge_fault: Some(drm_gem_shmem_huge_fault),
    map_pages: None,
    pagesize: None,
    page_mkwrite: None,
    pfn_mkwrite: Some(drm_gem_shmem_pfn_mkwrite),
    access: None,
    name: None,
    set_policy: None,
    get_policy: None,
    uffd_ops: ptr::null(),
});

unsafe extern "C" {
    fn vmf_insert_pfn(vma: *mut VmAreaStruct, address: usize, pfn: usize) -> u32;
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_record_mkwrite()
unsafe fn drm_gem_shmem_record_mkwrite(vmf: *mut VmFault) -> u32 {
    unsafe {
        let vma = (*vmf).vma;
        let obj = (*vma).vm_private_data.cast::<DrmGemObject>();
        let shmem = shmem_from_obj(obj);
        let page_offset = (*vmf).pgoff.wrapping_sub((*vma).vm_pgoff) as usize;
        if obj.is_null() || (*shmem).pages.is_null() || page_offset >= pages_count(shmem) {
            warn_on(true);
            return VM_FAULT_SIGBUS;
        }
        let page = *(*shmem).pages.add(page_offset);
        if page.is_null() {
            warn_on(true);
            return VM_FAULT_SIGBUS;
        }
        // The anonymous LinuxKPI shmem inode carries no timestamp fields;
        // dirty accounting on its page cache is the observable write state.
        folio_mark_dirty(page_folio(page));
        0
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_any_fault()
unsafe fn drm_gem_shmem_any_fault(vmf: *mut VmFault, order: u32) -> u32 {
    if order != 0 {
        return VM_FAULT_FALLBACK;
    }
    unsafe {
        let vma = (*vmf).vma;
        if vma.is_null() {
            return VM_FAULT_SIGBUS;
        }
        let obj = (*vma).vm_private_data.cast::<DrmGemObject>();
        if obj.is_null() {
            return VM_FAULT_SIGBUS;
        }
        let shmem = shmem_from_obj(obj);
        let page_offset = (*vmf).pgoff.wrapping_sub((*vma).vm_pgoff) as usize;
        if page_offset >= pages_count(shmem) || (*shmem).pages.is_null() || (*shmem).madv < 0 {
            return VM_FAULT_SIGBUS;
        }
        if dma_resv_lock((*obj).resv, ptr::null_mut()) != 0 {
            return VM_FAULT_SIGBUS;
        }
        let page = *(*shmem).pages.add(page_offset);
        if page.is_null() {
            dma_resv_unlock((*obj).resv);
            return VM_FAULT_SIGBUS;
        }
        let pfn = page_to_phys(page) >> PAGE_SHIFT;
        let result = vmf_insert_pfn(vma, (*vmf).address as usize, pfn);
        if result == VM_FAULT_NOPAGE {
            folio_mark_accessed(page_folio(page));
        }
        dma_resv_unlock((*obj).resv);
        result
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_fault()
unsafe extern "C" fn drm_gem_shmem_fault(vmf: *mut VmFault) -> u32 {
    unsafe { drm_gem_shmem_any_fault(vmf, 0) }
}

// CONFIG_ARCH_SUPPORTS_PMD_PFNMAP uses the same object/page validation. The
// V0 page-cache owner allocates order-0 pages, so huge faults request fallback.
unsafe extern "C" fn drm_gem_shmem_huge_fault(vmf: *mut VmFault, order: u32) -> u32 {
    unsafe { drm_gem_shmem_any_fault(vmf, order) }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_vm_open()
unsafe extern "C" fn drm_gem_shmem_vm_open(vma: *mut VmAreaStruct) {
    unsafe {
        let obj = (*vma).vm_private_data.cast::<DrmGemObject>();
        if obj.is_null() {
            warn_on(true);
            return;
        }
        let shmem = shmem_from_obj(obj);
        warn_on(shmem_is_imported(obj));
        if dma_resv_lock((*obj).resv, ptr::null_mut()) != 0 {
            warn_on(true);
            return;
        }
        warn_on(!refcount_inc_not_zero(&mut (*shmem).pages_use_count));
        dma_resv_unlock((*obj).resv);
        crate::drm_gem_upstream::drm_gem_object_get(obj);
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_vm_close()
unsafe extern "C" fn drm_gem_shmem_vm_close(vma: *mut VmAreaStruct) {
    unsafe {
        let obj = (*vma).vm_private_data.cast::<DrmGemObject>();
        if obj.is_null() {
            warn_on(true);
            return;
        }
        let shmem = shmem_from_obj(obj);
        if dma_resv_lock((*obj).resv, ptr::null_mut()) == 0 {
            drm_gem_shmem_put_pages_locked(shmem);
            dma_resv_unlock((*obj).resv);
        } else {
            warn_on(true);
        }
        crate::drm_gem_upstream::drm_gem_object_put(obj);
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_pfn_mkwrite()
unsafe extern "C" fn drm_gem_shmem_pfn_mkwrite(vmf: *mut VmFault) -> u32 {
    unsafe { drm_gem_shmem_record_mkwrite(vmf) }
}

// upstream: drm_gem_shmem_helper.c __drm_gem_shmem_init()
unsafe fn drm_gem_shmem_init_internal(
    dev: *mut DrmDevice,
    shmem: *mut DrmGemShmemObject,
    size: usize,
    private: bool,
) -> i32 {
    unsafe {
        let obj = ptr::addr_of_mut!((*shmem).base);
        if (*obj).funcs.is_null() {
            (*obj).funcs =
                &DRM_GEM_SHMEM_FUNCS.0 as *const DrmGemObjectFuncs as *mut DrmGemObjectFuncs;
        }

        if private {
            crate::drm_gem_upstream::drm_gem_private_object_init(dev, obj, size);
            shmem_set_flags(shmem, shmem_flags(shmem) & !DRM_GEM_SHMEM_MAP_WC);
        } else {
            let ret = crate::drm_gem_upstream::drm_gem_object_init(dev, obj, size);
            if ret != 0 {
                crate::drm_gem_upstream::drm_gem_private_object_fini(obj);
                return ret;
            }

            // The DRM object initializer is the common owner of this file in
            // Linux. The current lower-level GEM port initializes object
            // metadata only, so establish the real shmem mapping here.
            let file = shmem_file_setup(c"drm-gem-shmem".as_ptr(), size as u64, 0);
            if IS_ERR(file) {
                let ret = PTR_ERR(file);
                crate::drm_gem_upstream::drm_gem_private_object_fini(obj);
                return ret;
            }
            (*obj).filp = file.cast();
        }

        let ret = crate::drm_gem_upstream::drm_gem_create_mmap_offset(obj);
        if ret != 0 {
            if !(*obj).filp.is_null() {
                fput_file((*obj).filp.cast());
                (*obj).filp = ptr::null_mut();
            }
            crate::drm_gem_upstream::drm_gem_object_release(obj);
            return ret;
        }

        INIT_LIST_HEAD(ptr::addr_of_mut!((*shmem).madv_list));
        if !private {
            let mapping = shmem_mapping(shmem);
            if !mapping.is_null() {
                // This target has no movable zone. Preserve the source's
                // reclaim/retry/warning policy without inventing zones.
                tk_linux_kapi::shmem::mapping_set_gfp_mask(mapping, SHMEM_PAGES_GFP);
            }
        }
        0
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_init(
    dev: *mut DrmDevice,
    shmem: *mut DrmGemShmemObject,
    size: usize,
) -> i32 {
    unsafe { drm_gem_shmem_init_internal(dev, shmem, size, false) }
}

// upstream: drm_gem_shmem_helper.c __drm_gem_shmem_create()
unsafe fn drm_gem_shmem_create_internal(
    dev: *mut DrmDevice,
    size: usize,
    private: bool,
) -> *mut DrmGemShmemObject {
    let Some(size) = size
        .checked_add(PAGE_SIZE - 1)
        .map(|size| size & !(PAGE_SIZE - 1))
    else {
        return tk_linux_kapi::config::ERR_PTR(-EOVERFLOW);
    };
    let obj = unsafe {
        if !dev.is_null() && !(*dev).driver.is_null() {
            let driver = (*dev).driver.cast::<DrmDriver>();
            if let Some(create) = (*driver).gem_create_object {
                let gem = create(dev, size);
                if IS_ERR(gem) {
                    return gem.cast();
                }
                if gem.is_null() {
                    return tk_linux_kapi::config::ERR_PTR(-EINVAL);
                }
                shmem_from_obj(gem)
            } else {
                kzalloc_obj::<DrmGemShmemObject>()
            }
        } else {
            return tk_linux_kapi::config::ERR_PTR(-EINVAL);
        }
    };
    if obj.is_null() {
        return tk_linux_kapi::config::ERR_PTR(-tk_linux_kapi::config::ENOMEM);
    }

    let ret = unsafe { drm_gem_shmem_init_internal(dev, obj, size, private) };
    if ret != 0 {
        unsafe { kfree(obj.cast::<c_void>()) };
        return tk_linux_kapi::config::ERR_PTR(ret);
    }
    obj
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_create(
    dev: *mut DrmDevice,
    size: usize,
) -> *mut DrmGemShmemObject {
    unsafe { drm_gem_shmem_create_internal(dev, size, false) }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_get_pages_locked()
unsafe fn drm_gem_shmem_get_pages_locked(shmem: *mut DrmGemShmemObject) -> i32 {
    unsafe {
        let obj = ptr::addr_of_mut!((*shmem).base);
        if refcount_inc_not_zero(&mut (*shmem).pages_use_count) {
            return 0;
        }
        if shmem_is_imported(obj) {
            return -EINVAL;
        }

        let count = pages_count(shmem);
        if count == 0 || (*obj).filp.is_null() {
            return -EINVAL;
        }
        let Some(bytes) = count.checked_mul(core::mem::size_of::<*mut Page>()) else {
            return -EOVERFLOW;
        };
        let pages = kmalloc(bytes, GFP_KERNEL).cast::<*mut Page>();
        if pages.is_null() {
            return -tk_linux_kapi::config::ENOMEM;
        }
        ptr::write_bytes(pages, 0, count);

        let mapping = shmem_mapping(shmem);
        if mapping.is_null() {
            kfree(pages.cast::<c_void>());
            return -EINVAL;
        }
        for index in 0..count {
            let page = shmem_read_mapping_page_gfp(mapping, index as u64, SHMEM_PAGES_GFP);
            if page.is_null() || IS_ERR(page) {
                let ret = if IS_ERR(page) {
                    PTR_ERR(page)
                } else {
                    -tk_linux_kapi::config::ENOMEM
                };
                for prior in 0..index {
                    tk_linux_kapi::shmem::put_page(*pages.add(prior));
                }
                kfree(pages.cast::<c_void>());
                return ret;
            }
            *pages.add(index) = page;
        }

        if shmem_is_wc(shmem) {
            let Ok(count) = c_int::try_from(count) else {
                for index in 0..pages_count(shmem) {
                    tk_linux_kapi::shmem::put_page(*pages.add(index));
                }
                kfree(pages.cast::<c_void>());
                return -EOVERFLOW;
            };
            let ret = tk_linux_kapi::page_cache::set_pages_array_wc_kernel(pages, count);
            if ret != 0 {
                for index in 0..count as usize {
                    tk_linux_kapi::shmem::put_page(*pages.add(index));
                }
                kfree(pages.cast::<c_void>());
                return ret;
            }
        }

        (*shmem).pages = pages;
        refcount_set(&mut (*shmem).pages_use_count, 1);
        0
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_put_pages_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_put_pages_locked(shmem: *mut DrmGemShmemObject) {
    unsafe {
        if !refcount_dec_and_test(&mut (*shmem).pages_use_count) {
            return;
        }
        let count = pages_count(shmem);
        let pages = (*shmem).pages;
        if pages.is_null() {
            warn_on(true);
            return;
        }
        if shmem_is_wc(shmem) {
            let ret = c_int::try_from(count).map_or(-EOVERFLOW, |count| {
                tk_linux_kapi::page_cache::set_pages_array_wb_kernel(pages, count)
            });
            if ret != 0 {
                // Keep the pages referenced if restoring their direct-map
                // alias failed; freeing WC pages under a WB alias is unsafe.
                refcount_set(&mut (*shmem).pages_use_count, 1);
                warn_on(true);
                return;
            }
        }
        for index in 0..count {
            let page = *pages.add(index);
            if !page.is_null() {
                if shmem_flags(shmem) & DRM_GEM_SHMEM_PAGES_MARK_DIRTY_ON_PUT != 0 {
                    folio_mark_dirty(page_folio(page));
                }
                if shmem_flags(shmem) & DRM_GEM_SHMEM_PAGES_MARK_ACCESSED_ON_PUT != 0 {
                    folio_mark_accessed(page_folio(page));
                }
                tk_linux_kapi::shmem::put_page(page);
            }
        }
        kfree(pages.cast::<c_void>());
        (*shmem).pages = ptr::null_mut();
        shmem_set_flags(
            shmem,
            shmem_flags(shmem)
                & !(DRM_GEM_SHMEM_PAGES_MARK_DIRTY_ON_PUT
                    | DRM_GEM_SHMEM_PAGES_MARK_ACCESSED_ON_PUT),
        );
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_pin_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_pin_locked(shmem: *mut DrmGemShmemObject) -> i32 {
    unsafe {
        warn_on(shmem_is_imported(ptr::addr_of!((*shmem).base)));
        if refcount_inc_not_zero(&mut (*shmem).pages_pin_count) {
            return 0;
        }
        let ret = drm_gem_shmem_get_pages_locked(shmem);
        if ret == 0 {
            refcount_set(&mut (*shmem).pages_pin_count, 1);
        }
        ret
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_unpin_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_unpin_locked(shmem: *mut DrmGemShmemObject) {
    unsafe {
        if refcount_dec_and_test(&mut (*shmem).pages_pin_count) {
            drm_gem_shmem_put_pages_locked(shmem);
        }
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_pin()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_pin(shmem: *mut DrmGemShmemObject) -> i32 {
    unsafe {
        let obj = ptr::addr_of_mut!((*shmem).base);
        warn_on(shmem_is_imported(obj));
        if refcount_inc_not_zero(&mut (*shmem).pages_pin_count) {
            return 0;
        }
        let ret = dma_resv_lock_interruptible((*obj).resv, ptr::null_mut());
        if ret != 0 {
            return ret;
        }
        let ret = drm_gem_shmem_pin_locked(shmem);
        dma_resv_unlock((*obj).resv);
        ret
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_unpin()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_unpin(shmem: *mut DrmGemShmemObject) {
    unsafe {
        let obj = ptr::addr_of_mut!((*shmem).base);
        warn_on(shmem_is_imported(obj));
        if refcount_dec_not_one(ptr::addr_of_mut!((*shmem).pages_pin_count)) {
            return;
        }
        if dma_resv_lock((*obj).resv, ptr::null_mut()) == 0 {
            drm_gem_shmem_unpin_locked(shmem);
            dma_resv_unlock((*obj).resv);
        }
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_vmap_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_vmap_locked(
    shmem: *mut DrmGemShmemObject,
    map: *mut IosysMap,
) -> i32 {
    unsafe {
        let obj = ptr::addr_of_mut!((*shmem).base);
        if map.is_null() || (*obj).resv.is_null() {
            return -EINVAL;
        }
        if shmem_is_imported(obj) {
            let attach = (*obj).import_attach.cast::<DmaBufAttachment>();
            let dmabuf = (*attach).dmabuf;
            let ops = (*dmabuf).ops;
            if ops.is_null() || (*ops).vmap.is_none() {
                return -EOPNOTSUPP;
            }
            return (*ops).vmap.unwrap()(dmabuf, map.cast());
        }

        if refcount_inc_not_zero(&mut (*shmem).vmap_use_count) {
            (*map).is_iomem = false;
            (*map)._anon_0 = IosysMapAnon0 {
                vaddr: (*shmem).vaddr,
            };
            return 0;
        }
        let ret = drm_gem_shmem_pin_locked(shmem);
        if ret != 0 {
            return ret;
        }
        let prot = if shmem_is_wc(shmem) {
            PAGE_KERNEL_WC
        } else {
            PAGE_KERNEL
        };
        let Ok(count) = u32::try_from(pages_count(shmem)) else {
            drm_gem_shmem_unpin_locked(shmem);
            return -EOVERFLOW;
        };
        let address = vmap((*shmem).pages, count, 0, prot);
        if address.is_null() {
            drm_gem_shmem_unpin_locked(shmem);
            return -tk_linux_kapi::config::ENOMEM;
        }
        (*shmem).vaddr = address;
        (*map).is_iomem = false;
        (*map)._anon_0 = IosysMapAnon0 { vaddr: address };
        refcount_set(&mut (*shmem).vmap_use_count, 1);
        shmem_set_flags(
            shmem,
            shmem_flags(shmem)
                | DRM_GEM_SHMEM_PAGES_MARK_ACCESSED_ON_PUT
                | DRM_GEM_SHMEM_PAGES_MARK_DIRTY_ON_PUT,
        );
        0
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_vunmap_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_vunmap_locked(
    shmem: *mut DrmGemShmemObject,
    map: *mut IosysMap,
) {
    unsafe {
        let obj = ptr::addr_of_mut!((*shmem).base);
        if shmem_is_imported(obj) {
            let attach = (*obj).import_attach.cast::<DmaBufAttachment>();
            let dmabuf = (*attach).dmabuf;
            if let Some(vunmap) = (*(*dmabuf).ops).vunmap {
                vunmap(dmabuf, map.cast());
            }
            return;
        }
        if refcount_dec_and_test(&mut (*shmem).vmap_use_count) {
            vunmap((*shmem).vaddr);
            (*shmem).vaddr = ptr::null_mut();
            drm_gem_shmem_unpin_locked(shmem);
        }
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_madvise_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_madvise_locked(
    shmem: *mut DrmGemShmemObject,
    madv: c_int,
) -> c_int {
    unsafe {
        if (*shmem).madv >= 0 {
            (*shmem).madv = madv;
        }
        if (*shmem).madv >= 0 { 1 } else { 0 }
    }
}

#[inline]
unsafe fn drm_gem_shmem_is_purgeable(shmem: *mut DrmGemShmemObject) -> bool {
    unsafe {
        (*shmem).madv > 0
            && refcount_read(&(*shmem).pages_pin_count) == 0
            && !(*shmem).sgt.is_null()
            && (*shmem).base.dma_buf.is_null()
            && !shmem_is_imported(ptr::addr_of!((*shmem).base))
    }
}

// upstream: drm_gem_shmem_helper.c __drm_gem_shmem_free_sgt_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_gem_shmem_free_sgt_locked(shmem: *mut DrmGemShmemObject) {
    unsafe {
        let sgt = (*shmem).sgt;
        if sgt.is_null() {
            return;
        }
        let dev = (*(*shmem).base.dev).dev;
        tk_linux_kapi::dma::dma_unmap_sg_attrs(
            dev,
            (*sgt).sgl,
            (*sgt).orig_nents,
            DMA_BIDIRECTIONAL as u32,
            0,
        );
        sg_free_table(sgt);
        kfree(sgt.cast::<c_void>());
        (*shmem).sgt = ptr::null_mut();
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_purge_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_purge_locked(shmem: *mut DrmGemShmemObject) {
    unsafe {
        let obj = ptr::addr_of_mut!((*shmem).base);
        warn_on(!drm_gem_shmem_is_purgeable(shmem));
        if !(*shmem).sgt.is_null() {
            __drm_gem_shmem_free_sgt_locked(shmem);
        }
        if !(*shmem).pages.is_null() {
            drm_gem_shmem_put_pages_locked(shmem);
        }
        (*shmem).madv = DRM_GEM_SHMEM_PURGED;
        crate::drm_gem_upstream::drm_gem_free_mmap_offset(obj);
        if !(*obj).filp.is_null() {
            let inode = (*(*obj).filp).f_inode.cast::<tk_linux_kapi::shmem::Inode>();
            shmem_truncate_range(inode, 0, -1);
        }
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_dumb_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_dumb_create(
    file: *mut DrmFile,
    dev: *mut DrmDevice,
    args: *mut DrmModeCreateDumb,
) -> i32 {
    unsafe {
        let ret = drm_mode_size_dumb(dev, args, 0, 0);
        if ret != 0 {
            return ret;
        }
        let shmem = drm_gem_shmem_create(dev, (*args).size as usize);
        if IS_ERR(shmem) {
            return PTR_ERR(shmem);
        }
        let ret = crate::drm_gem_upstream::drm_gem_handle_create(
            file,
            ptr::addr_of_mut!((*shmem).base),
            ptr::addr_of_mut!((*args).handle),
        );
        crate::drm_gem_upstream::drm_gem_object_put(ptr::addr_of_mut!((*shmem).base));
        ret
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_mmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_mmap(
    shmem: *mut DrmGemShmemObject,
    vma: *mut tk_linux_kapi::mm::VmAreaStruct,
) -> i32 {
    unsafe {
        let obj = ptr::addr_of_mut!((*shmem).base);
        if shmem_is_imported(obj) {
            (*vma).vm_private_data = ptr::null_mut();
            (*vma).vm_ops = ptr::null();
            let dmabuf = (*obj).dma_buf.cast::<DmaBuf>();
            if dmabuf.is_null() || (*dmabuf).ops.is_null() {
                return -EOPNOTSUPP;
            }
            let Some(mmap) = (*(*dmabuf).ops).mmap else {
                return -EOPNOTSUPP;
            };
            let ret = mmap(dmabuf, vma.cast());
            if ret == 0 {
                crate::drm_gem_upstream::drm_gem_object_put(obj);
            }
            return ret;
        }
        let flags = (*vma).vm_flags;
        if tk_linux_kapi::mm::is_cow_mapping(flags) {
            return -EINVAL;
        }

        let ret = dma_resv_lock((*obj).resv, ptr::null_mut());
        if ret != 0 {
            return ret;
        }
        let ret = drm_gem_shmem_get_pages_locked(shmem);
        dma_resv_unlock((*obj).resv);
        if ret != 0 {
            return ret;
        }

        tk_linux_kapi::mm::vm_flags_set(vma, VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
        (*vma).vm_page_prot = tk_linux_kapi::mm::vm_get_page_prot((*vma).vm_flags);
        if shmem_is_wc(shmem) {
            (*vma).vm_page_prot = tk_linux_kapi::vm::pgprot_writecombine((*vma).vm_page_prot);
        }
        (*vma).vm_private_data = obj.cast();
        (*vma).vm_ops = &DRM_GEM_SHMEM_VM_OPS.0;
        0
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_print_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_print_info(
    shmem: *const DrmGemShmemObject,
    printer: *mut DrmPrinter,
    indent: c_uint,
) {
    unsafe {
        if shmem_is_imported(ptr::addr_of!((*shmem).base)) {
            return;
        }
        let pad = "\t".repeat(indent as usize);
        let text = alloc::format!(
            r#"{pad}pages_pin_count={}
{pad}pages_use_count={}
{pad}vmap_use_count={}
{pad}vaddr={:p}
"#,
            refcount_read(&(*shmem).pages_pin_count),
            refcount_read(&(*shmem).pages_use_count),
            refcount_read(&(*shmem).vmap_use_count),
            (*shmem).vaddr,
        );
        crate::drm_print_upstream::drm_printf(printer, &text);
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_get_sg_table()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_get_sg_table(shmem: *mut DrmGemShmemObject) -> *mut SgTable {
    unsafe {
        let obj = ptr::addr_of_mut!((*shmem).base);
        warn_on(shmem_is_imported(obj));
        if shmem_is_imported(obj) || (*shmem).pages.is_null() {
            return tk_linux_kapi::config::ERR_PTR(-EINVAL);
        }
        let count = pages_count(shmem);
        let Ok(count) = u32::try_from(count) else {
            return tk_linux_kapi::config::ERR_PTR(-EOVERFLOW);
        };
        let sgt = kzalloc_obj::<SgTable>();
        if sgt.is_null() {
            return tk_linux_kapi::config::ERR_PTR(-tk_linux_kapi::config::ENOMEM);
        }
        let ret = sg_alloc_table_from_pages_segment(
            sgt,
            (*shmem).pages,
            count,
            0,
            (*obj).size as u64,
            u32::MAX,
            GFP_KERNEL,
        );
        if ret != 0 {
            kfree(sgt);
            return tk_linux_kapi::config::ERR_PTR(ret);
        }
        sgt
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_get_pages_sgt_locked()
unsafe fn drm_gem_shmem_get_pages_sgt_locked(shmem: *mut DrmGemShmemObject) -> *mut SgTable {
    unsafe {
        if !(*shmem).sgt.is_null() {
            return (*shmem).sgt;
        }
        if shmem_is_imported(ptr::addr_of!((*shmem).base)) {
            return tk_linux_kapi::config::ERR_PTR(-EINVAL);
        }
        let ret = drm_gem_shmem_get_pages_locked(shmem);
        if ret != 0 {
            return tk_linux_kapi::config::ERR_PTR(ret);
        }
        let sgt = drm_gem_shmem_get_sg_table(shmem);
        if IS_ERR(sgt) {
            let ret = PTR_ERR(sgt);
            drm_gem_shmem_put_pages_locked(shmem);
            return tk_linux_kapi::config::ERR_PTR(ret);
        }
        let ret = dma_map_sgtable((*(*shmem).base.dev).dev, sgt, DMA_BIDIRECTIONAL, 0);
        if ret != 0 {
            sg_free_table(sgt);
            kfree(sgt.cast::<c_void>());
            drm_gem_shmem_put_pages_locked(shmem);
            return tk_linux_kapi::config::ERR_PTR(ret);
        }
        (*shmem).sgt = sgt;
        sgt
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_get_pages_sgt()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_get_pages_sgt(
    shmem: *mut DrmGemShmemObject,
) -> *mut SgTable {
    unsafe {
        let ret = dma_resv_lock_interruptible((*shmem).base.resv, ptr::null_mut());
        if ret != 0 {
            return tk_linux_kapi::config::ERR_PTR(ret);
        }
        let sgt = drm_gem_shmem_get_pages_sgt_locked(shmem);
        dma_resv_unlock((*shmem).base.resv);
        sgt
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_prime_import_sg_table()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_prime_import_sg_table(
    dev: *mut DrmDevice,
    attach: *mut DmaBufAttachment,
    sgt: *mut SgTable,
) -> *mut DrmGemObject {
    unsafe {
        if attach.is_null() || (*attach).dmabuf.is_null() {
            return tk_linux_kapi::config::ERR_PTR(-EINVAL);
        }
        let Some(size) = (*(*attach).dmabuf).size.checked_add(PAGE_SIZE - 1) else {
            return tk_linux_kapi::config::ERR_PTR(-EOVERFLOW);
        };
        let shmem = drm_gem_shmem_create_internal(dev, size & !(PAGE_SIZE - 1), true);
        if IS_ERR(shmem) {
            return shmem.cast();
        }
        (*shmem).sgt = sgt;
        ptr::addr_of_mut!((*shmem).base)
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_prime_import_no_map()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_prime_import_no_map(
    dev: *mut DrmDevice,
    dmabuf: *mut DmaBuf,
) -> *mut DrmGemObject {
    unsafe {
        if dev.is_null() || dmabuf.is_null() {
            return tk_linux_kapi::config::ERR_PTR(-EINVAL);
        }
        if crate::drm_prime_upstream::drm_gem_is_prime_exported_dma_buf(dev, dmabuf) {
            let obj = (*dmabuf).r#priv.cast::<DrmGemObject>();
            crate::drm_gem_upstream::drm_gem_object_get(obj);
            return obj;
        }

        let attach = dma_buf_attach(dmabuf, (*dev).dev);
        if IS_ERR(attach) {
            return attach.cast();
        }
        if attach.is_null() {
            return tk_linux_kapi::config::ERR_PTR(-EIO);
        }
        crate::drm_prime_upstream::get_dma_buf(dmabuf);
        let Some(size) = (*dmabuf).size.checked_add(PAGE_SIZE - 1) else {
            dma_buf_detach(dmabuf, attach);
            dma_buf_put(dmabuf);
            return tk_linux_kapi::config::ERR_PTR(-EOVERFLOW);
        };
        let shmem = drm_gem_shmem_create_internal(dev, size & !(PAGE_SIZE - 1), true);
        if IS_ERR(shmem) {
            let ret = PTR_ERR(shmem);
            dma_buf_detach(dmabuf, attach);
            dma_buf_put(dmabuf);
            return tk_linux_kapi::config::ERR_PTR(ret);
        }
        (*shmem).base.import_attach = attach.cast();
        (*shmem).base.resv = (*dmabuf).resv;
        ptr::addr_of_mut!((*shmem).base)
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_release(shmem: *mut DrmGemShmemObject) {
    unsafe {
        let obj = ptr::addr_of_mut!((*shmem).base);
        if shmem_is_imported(obj) {
            let attach = (*obj).import_attach.cast::<DmaBufAttachment>();
            if !(*shmem).sgt.is_null() {
                dma_buf_unmap_attachment_unlocked(attach, (*shmem).sgt, DMA_BIDIRECTIONAL);
                (*shmem).sgt = ptr::null_mut();
            }
            crate::drm_prime_upstream::drm_prime_gem_destroy(obj, (*shmem).sgt);
        } else {
            let ret = dma_resv_lock((*obj).resv, ptr::null_mut());
            if ret != 0 {
                warn_on(true);
                return;
            }
            warn_on(refcount_read(&(*shmem).vmap_use_count) != 0);
            if !(*shmem).sgt.is_null() {
                __drm_gem_shmem_free_sgt_locked(shmem);
            }
            if !(*shmem).pages.is_null() {
                drm_gem_shmem_put_pages_locked(shmem);
            }
            warn_on(refcount_read(&(*shmem).pages_use_count) != 0);
            warn_on(refcount_read(&(*shmem).pages_pin_count) != 0);
            dma_resv_unlock((*obj).resv);
        }
        if !(*obj).filp.is_null() {
            fput_file((*obj).filp.cast());
            (*obj).filp = ptr::null_mut();
        }
        crate::drm_gem_upstream::drm_gem_object_release(obj);
    }
}

// upstream: drm_gem_shmem_helper.c drm_gem_shmem_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_free(shmem: *mut DrmGemShmemObject) {
    unsafe {
        drm_gem_shmem_release(shmem);
        kfree(shmem);
    }
}

// GEM callback wrappers from include/drm/drm_gem_shmem_helper.h.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_object_free(obj: *mut DrmGemObject) {
    unsafe { drm_gem_shmem_free(shmem_from_obj(obj)) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_object_print_info(
    printer: *mut DrmPrinter,
    indent: c_uint,
    obj: *mut DrmGemObject,
) {
    unsafe { drm_gem_shmem_print_info(shmem_from_obj(obj), printer, indent) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_object_pin(obj: *mut DrmGemObject) -> i32 {
    unsafe { drm_gem_shmem_pin_locked(shmem_from_obj(obj)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_object_unpin(obj: *mut DrmGemObject) {
    unsafe { drm_gem_shmem_unpin_locked(shmem_from_obj(obj)) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_object_get_sg_table(obj: *mut DrmGemObject) -> *mut SgTable {
    unsafe { drm_gem_shmem_get_sg_table(shmem_from_obj(obj)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_object_vmap(
    obj: *mut DrmGemObject,
    map: *mut IosysMap,
) -> i32 {
    unsafe { drm_gem_shmem_vmap_locked(shmem_from_obj(obj), map) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_object_vunmap(obj: *mut DrmGemObject, map: *mut IosysMap) {
    unsafe { drm_gem_shmem_vunmap_locked(shmem_from_obj(obj), map) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_shmem_object_mmap(
    obj: *mut DrmGemObject,
    vma: *mut c_void,
) -> i32 {
    unsafe { drm_gem_shmem_mmap(shmem_from_obj(obj), vma.cast()) }
}

// Linux vmap/vunmap/madvise/purge entry points are exported for KUnit only.
#[cfg(test)]
pub unsafe fn drm_gem_shmem_vmap(shmem: *mut DrmGemShmemObject, map: *mut IosysMap) -> i32 {
    unsafe {
        let ret = dma_resv_lock_interruptible((*shmem).base.resv, ptr::null_mut());
        if ret != 0 {
            return ret;
        }
        let ret = drm_gem_shmem_vmap_locked(shmem, map);
        dma_resv_unlock((*shmem).base.resv);
        ret
    }
}

#[cfg(test)]
pub unsafe fn drm_gem_shmem_vunmap(shmem: *mut DrmGemShmemObject, map: *mut IosysMap) {
    unsafe {
        if dma_resv_lock_interruptible((*shmem).base.resv, ptr::null_mut()) == 0 {
            drm_gem_shmem_vunmap_locked(shmem, map);
            dma_resv_unlock((*shmem).base.resv);
        }
    }
}

#[cfg(test)]
pub unsafe fn drm_gem_shmem_madvise(shmem: *mut DrmGemShmemObject, madv: c_int) -> c_int {
    unsafe {
        let ret = dma_resv_lock_interruptible((*shmem).base.resv, ptr::null_mut());
        if ret != 0 {
            return ret;
        }
        let ret = drm_gem_shmem_madvise_locked(shmem, madv);
        dma_resv_unlock((*shmem).base.resv);
        ret
    }
}

#[cfg(test)]
pub unsafe fn drm_gem_shmem_purge(shmem: *mut DrmGemShmemObject) -> i32 {
    unsafe {
        let ret = dma_resv_lock_interruptible((*shmem).base.resv, ptr::null_mut());
        if ret != 0 {
            return ret;
        }
        drm_gem_shmem_purge_locked(shmem);
        dma_resv_unlock((*shmem).base.resv);
        0
    }
}

#[cfg(test)]
mod tests {
    use std::boxed::Box;

    use tk_linux_kapi::config::PAGE_SIZE;

    use super::*;
    use crate::drm_vma_manager_types::DrmVmaOffsetManager;

    #[test]
    fn pin_pages_and_scatterlist_keep_independent_lifetimes() {
        unsafe {
            let mut test_dev = crate::test_support::TestDrm::new(0);
            let mut manager: Box<DrmVmaOffsetManager> = Box::new(core::mem::zeroed());
            crate::drm_vma_manager_upstream::drm_vma_offset_manager_init(
                &mut *manager,
                0x1000,
                0x10000,
            );
            test_dev.dev.vma_offset_manager = &mut *manager;

            let shmem = drm_gem_shmem_create(test_dev.ptr(), 2 * PAGE_SIZE);
            assert!(!IS_ERR(shmem));
            assert_eq!(drm_gem_shmem_pin_locked(shmem), 0);
            assert_eq!(refcount_read(&(*shmem).pages_use_count), 1);
            assert_eq!(refcount_read(&(*shmem).pages_pin_count), 1);

            // A second pin changes only the pin refcount; both pins share the
            // same page array, while the caller owns its own SGT container.
            assert_eq!(drm_gem_shmem_pin_locked(shmem), 0);
            assert_eq!(refcount_read(&(*shmem).pages_use_count), 1);
            assert_eq!(refcount_read(&(*shmem).pages_pin_count), 2);
            let sgt = drm_gem_shmem_get_sg_table(shmem);
            assert!(!IS_ERR(sgt));
            assert!((*sgt).orig_nents >= 1 && (*sgt).orig_nents <= 2);
            sg_free_table(sgt);
            kfree(sgt);

            drm_gem_shmem_unpin_locked(shmem);
            assert_eq!(refcount_read(&(*shmem).pages_use_count), 1);
            assert!(!(*shmem).pages.is_null());
            drm_gem_shmem_unpin_locked(shmem);
            assert_eq!(refcount_read(&(*shmem).pages_use_count), 0);
            assert_eq!(refcount_read(&(*shmem).pages_pin_count), 0);
            assert!((*shmem).pages.is_null());

            drm_gem_shmem_free(shmem);
            crate::drm_vma_manager_upstream::drm_vma_offset_manager_destroy(&mut *manager);
        }
    }

    #[test]
    fn failed_dma_mapping_releases_the_temporary_sgt_and_page_pin() {
        unsafe {
            let mut test_dev = crate::test_support::TestDrm::new(0);
            let mut manager: Box<DrmVmaOffsetManager> = Box::new(core::mem::zeroed());
            crate::drm_vma_manager_upstream::drm_vma_offset_manager_init(
                &mut *manager,
                0x1000,
                0x10000,
            );
            test_dev.dev.vma_offset_manager = &mut *manager;

            let shmem = drm_gem_shmem_create(test_dev.ptr(), 2 * PAGE_SIZE);
            assert!(!IS_ERR(shmem));
            // TestDrm intentionally has no registered DMA device. The DMA
            // layer rejects this mapping instead of inventing an address.
            let sgt = drm_gem_shmem_get_pages_sgt(shmem);
            assert!(IS_ERR(sgt));
            assert_eq!(refcount_read(&(*shmem).pages_use_count), 0);
            assert!((*shmem).pages.is_null());
            assert!((*shmem).sgt.is_null());

            drm_gem_shmem_free(shmem);
            crate::drm_vma_manager_upstream::drm_vma_offset_manager_destroy(&mut *manager);
        }
    }
}
