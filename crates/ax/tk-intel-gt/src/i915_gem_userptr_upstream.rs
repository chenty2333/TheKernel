// SPDX-License-Identifier: MIT
// Copyright © 2012-2014 Intel Corporation.
// Copyright © 2014 Advanced Micro Devices, Inc.
//
//! Linux v7.2.3 `drivers/gpu/drm/i915/gem/i915_gem_userptr.c` translation.
//!
//! The source SPDX is MIT (including its stated AMD-origin MIT grant). This
//! module uses the canonical `DrmI915GemObject`/userptr storage and LinuxKPI
//! MMU-notifier record. Remaining Linux MM, Maple VMA iterator, scatterlist,
//! current-task-mm, DMA-device, and reservation-lock services are external
//! framework boundaries; this file stays unregistered until their owners
//! provide the exact APIs. No fake by-value MM/VM/page record is introduced.
#![allow(unsafe_code)]

use core::{
    ffi::{c_char, c_long, c_ulong, c_void},
    mem::size_of,
};

use crate::{
    i915_gem_object_types_upstream::{
        DrmI915GemObject, DrmI915GemObjectOps, I915_BO_ALLOC_USER,
        I915_BO_CACHE_COHERENT_FOR_WRITE, I915_BO_FLAG_STRUCT_PAGE, I915_BO_READONLY,
        I915_GEM_OBJECT_IS_PROXY, I915_GEM_OBJECT_IS_SHRINKABLE, I915_GEM_OBJECT_NO_MMAP,
        I915GemObjectUserptr, Page,
    },
    i915_gem_shmem_upstream::{DrmI915GemPread, DrmI915GemPwrite},
    i915_request_types_upstream::DrmFile,
    intel_context_upstream::{DmaFence, DrmVmaOffsetNode, Kref, SgTable},
    intel_gt_types_upstream::IntelGt,
    linux::{
        gem::DrmGemObject,
        mmu_notifier::{MmStruct, MmuIntervalNotifier, MmuIntervalNotifierOps, MmuNotifierRange},
    },
    linux_i915_private::DrmI915Private,
};

const PAGE_SHIFT: u32 = 12;
const I915_GEM_DOMAIN_CPU: u32 = 1;
const I915_GEM_OBJECT_UNBIND_ACTIVE: c_ulong = 1 << 0;
const I915_USERPTR_READ_ONLY: u32 = 1 << 0;
const I915_USERPTR_PROBE: u32 = 1 << 1;
const I915_USERPTR_UNSYNCHRONIZED: u32 = 1 << 31;
const FOLL_WRITE: u32 = 1 << 0;
const EALREADY: i32 = 114;

/// UAPI `struct drm_i915_gem_userptr` used by the ioctl (`include/uapi/drm/i915_drm.h`).
#[repr(C)]
pub struct DrmI915GemUserptr {
    pub user_ptr: u64,
    pub user_size: u64,
    pub flags: u32,
    pub handle: u32,
}

#[repr(C)]
pub struct LockClassKey {
    _opaque: [u8; 0],
}

static mut I915_GEM_USERPTR_LOCK_CLASS: LockClassKey = LockClassKey { _opaque: [] };

unsafe extern "C" {
    fn i915_gem_object_alloc() -> *mut DrmI915GemObject;
    fn i915_gem_object_init(
        obj: *mut DrmI915GemObject,
        ops: *const DrmI915GemObjectOps,
        key: *mut LockClassKey,
        flags: u32,
    );
    fn i915_gem_object_set_cache_coherency(obj: *mut DrmI915GemObject, cache_level: u32);
    fn i915_gem_object_unbind(obj: *mut DrmI915GemObject, flags: c_ulong) -> i32;
    fn __i915_gem_object_unset_pages(obj: *mut DrmI915GemObject) -> *mut SgTable;
    fn __i915_gem_object_set_pages(obj: *mut DrmI915GemObject, pages: *mut SgTable);
    fn __i915_gem_object_release_shmem(
        obj: *mut DrmI915GemObject,
        pages: *mut SgTable,
        mark_dirty: bool,
    );
    fn i915_gem_object_gtt_prepare_pages(obj: *mut DrmI915GemObject, pages: *mut SgTable) -> i32;
    pub(crate) fn i915_gem_gtt_finish_pages(obj: *mut DrmI915GemObject, pages: *mut SgTable);
    fn i915_gem_object_can_bypass_llc(obj: *mut DrmI915GemObject) -> bool;
    fn ____i915_gem_object_get_pages(obj: *mut DrmI915GemObject) -> i32;
    pub(crate) fn drm_gem_private_object_init(dev: *mut c_void, obj: *mut DrmGemObject, size: usize);
    fn drm_gem_handle_create(file: *mut DrmFile, obj: *mut DrmGemObject, handle: *mut u32) -> i32;
    fn drm_gem_object_free(refcount: *mut Kref);
    fn sg_alloc_table_from_pages_segment(
        sgt: *mut SgTable,
        pages: *mut *mut Page,
        n_pages: u32,
        offset: u32,
        size: c_ulong,
        max_segment: u32,
        gfp_mask: u32,
    ) -> i32;
    pub(crate) fn sg_free_table(sgt: *mut SgTable);
    fn pin_user_pages_fast(
        start: c_ulong,
        nr_pages: i32,
        gup_flags: u32,
        pages: *mut *mut Page,
    ) -> i32;
    fn unpin_user_pages(pages: *mut *mut Page, npages: c_ulong);
    fn set_page_dirty(page: *mut Page) -> bool;
    fn mark_page_accessed(page: *mut Page);
    fn unlock_page(page: *mut Page);
    fn ww_mutex_lock_interruptible(lock: *mut c_void, ctx: *mut c_void) -> i32;
    fn ww_mutex_unlock(lock: *mut c_void);
    fn kvfree(pointer: *mut c_void);
}

unsafe extern "C" fn drm_gem_object_release(refcount: *mut Kref) {
    // SAFETY: final DRM GEM reference, matching drm_gem_object_put().
    unsafe { drm_gem_object_free(refcount) }
}

/// The DRM GEM subobject is the first field of the source's anonymous base union.
#[inline]
unsafe fn gem_base(obj: *mut DrmI915GemObject) -> *mut DrmGemObject {
    unsafe { core::ptr::addr_of_mut!((*obj).base).cast::<DrmGemObject>() }
}

#[inline]
unsafe fn userptr_record(obj: *mut DrmI915GemObject) -> *mut I915GemObjectUserptr {
    unsafe { core::ptr::addr_of_mut!((*obj).backing.userptr).cast::<I915GemObjectUserptr>() }
}

#[inline]
unsafe fn object_put(obj: *mut DrmI915GemObject) {
    let base = unsafe { gem_base(obj) };
    unsafe { crate::linux::memory::kref_put(base.cast::<Kref>(), drm_gem_object_release) };
}

#[inline]
unsafe fn object_lock_interruptible(obj: *mut DrmI915GemObject) -> i32 {
    let resv = unsafe { (*gem_base(obj)).resv };
    let ret = unsafe { ww_mutex_lock_interruptible(resv, core::ptr::null_mut()) };
    if ret == -EALREADY { 0 } else { ret }
}

#[inline]
unsafe fn object_unlock(obj: *mut DrmI915GemObject) {
    let ops = unsafe { (*obj).ops };
    if let Some(adjust_lru) = unsafe { (*ops).adjust_lru } {
        unsafe { adjust_lru(obj) };
    }
    unsafe { ww_mutex_unlock((*gem_base(obj)).resv) };
}

#[inline]
unsafe fn object_has_pinned_pages(obj: *const DrmI915GemObject) -> bool {
    unsafe { crate::linux::memory::atomic_read(&(*obj).mm.pages_pin_count) != 0 }
}

#[inline]
unsafe fn object_readonly(obj: *const DrmI915GemObject) -> bool {
    unsafe { (*obj).flags & I915_BO_READONLY != 0 }
}

#[inline]
unsafe fn cache_coherent(obj: *const DrmI915GemObject) -> u32 {
    unsafe { ((*obj).cache_state_bits >> 7) & 0x3 }
}

#[inline]
unsafe fn set_cache_dirty(obj: *mut DrmI915GemObject, dirty: bool) {
    let bits = unsafe { &mut (*obj).cache_state_bits };
    if dirty {
        *bits |= 1 << 9;
    } else {
        *bits &= !(1 << 9);
    }
}

#[inline]
unsafe fn object_is_dirty(obj: *const DrmI915GemObject) -> bool {
    unsafe { (*obj).cache_state_bits & (1 << 9) != 0 }
}

#[inline]
unsafe fn userptr_page_ref(obj: *mut DrmI915GemObject) -> i32 {
    unsafe { (*userptr_record(obj)).page_ref }
}

#[inline]
unsafe fn userptr_set_page_ref(obj: *mut DrmI915GemObject, value: i32) {
    unsafe { (*userptr_record(obj)).page_ref = value }
}

// upstream: i915_gem_userptr.c i915_gem_userptr_invalidate()
unsafe extern "C" fn i915_gem_userptr_invalidate(
    mni: *mut MmuIntervalNotifier,
    _range: *const MmuNotifierRange,
    cur_seq: c_ulong,
) -> bool {
    // Source mmu_interval_set_seq() is WRITE_ONCE on invalidate_seq.
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!((*mni).invalidate_seq), cur_seq) };
    true
}

static I915_GEM_USERPTR_NOTIFIER_OPS: MmuIntervalNotifierOps = MmuIntervalNotifierOps {
    invalidate: Some(i915_gem_userptr_invalidate),
    invalidate_start: None,
    invalidate_finish: None,
};

#[inline]
fn mmu_interval_read_retry(mni: *const MmuIntervalNotifier, seq: c_ulong) -> bool {
    unsafe { (*mni).invalidate_seq != seq }
}

unsafe extern "C" {
    fn mmu_interval_read_begin(mni: *mut MmuIntervalNotifier) -> c_ulong;
    fn mmu_interval_notifier_insert(
        mni: *mut MmuIntervalNotifier,
        mm: *mut MmStruct,
        start: c_ulong,
        length: c_ulong,
        ops: *const MmuIntervalNotifierOps,
    ) -> i32;
    fn mmu_interval_notifier_remove(mni: *mut MmuIntervalNotifier);
}

// upstream: i915_gem_userptr.c i915_gem_userptr_init__mmu_notifier()
unsafe fn i915_gem_userptr_init__mmu_notifier(obj: *mut DrmI915GemObject) -> i32 {
    let userptr = unsafe { userptr_record(obj) };
    let mm = crate::linux::mmu_notifier::current_mm();
    let base = unsafe { gem_base(obj) };
    unsafe {
        mmu_interval_notifier_insert(
            core::ptr::addr_of_mut!((*userptr).notifier),
            mm,
            (*userptr).ptr as c_ulong,
            (*base).size as c_ulong,
            core::ptr::addr_of!(I915_GEM_USERPTR_NOTIFIER_OPS),
        )
    }
}

// upstream: i915_gem_userptr.c i915_gem_object_userptr_drop_ref()
unsafe fn i915_gem_object_userptr_drop_ref(obj: *mut DrmI915GemObject) {
    // assert_object_held_shared() compiles away for the target LOCKDEP=n.
    let userptr = unsafe { userptr_record(obj) };
    let refs = unsafe { (*userptr).page_ref.wrapping_sub(1) };
    unsafe { (*userptr).page_ref = refs };
    let pvec = if refs == 0 {
        let pvec = unsafe { (*userptr).pvec };
        unsafe { (*userptr).pvec = core::ptr::null_mut() };
        pvec
    } else {
        core::ptr::null_mut()
    };
    GEM_BUG_ON!(refs < 0);

    if !pvec.is_null() {
        let size = unsafe { (*gem_base(obj)).size };
        let num_pages = size >> PAGE_SHIFT;
        unsafe { unpin_user_pages(pvec, num_pages as c_ulong) };
        unsafe { kvfree(pvec.cast::<c_void>()) };
    }
}

// upstream: i915_gem_userptr.c i915_gem_userptr_get_pages()
unsafe extern "C" fn i915_gem_userptr_get_pages(obj: *mut DrmI915GemObject) -> i32 {
    let base = unsafe { gem_base(obj) };
    // The DRM device's embedded `struct device` and DMA segment helper are
    // owned by the missing Linux DRM/scatterlist framework bindings.
    let dev = crate::linux::gem::drm_device_device(unsafe { (*base).dev });
    let mut max_segment = crate::linux::scatterlist::i915_sg_segment_size(dev);
    let mut st = crate::linux::memory::kmalloc_obj::<SgTable>(crate::linux_config::GFP_KERNEL);
    let size = unsafe { (*base).size };
    let pages_u64 = size >> PAGE_SHIFT;
    if pages_u64 > u32::MAX as u64 {
        return -crate::linux_config::E2BIG;
    }
    let num_pages = pages_u64 as u32;
    if st.is_null() {
        return -crate::linux_config::ENOMEM;
    }

    let userptr = unsafe { userptr_record(obj) };
    if unsafe { (*userptr).page_ref == 0 } {
        unsafe { crate::linux::memory::kfree(st) };
        return -crate::linux_config::EAGAIN;
    }

    unsafe { (*userptr).page_ref = (*userptr).page_ref.wrapping_add(1) };
    let pvec = unsafe { (*userptr).pvec };
    let mut ret;

    loop {
        ret = unsafe {
            sg_alloc_table_from_pages_segment(
                st,
                pvec,
                num_pages,
                0,
                num_pages.wrapping_shl(PAGE_SHIFT) as c_ulong,
                max_segment,
                crate::linux_config::GFP_KERNEL,
            )
        };
        if ret != 0 {
            break;
        }

        ret = unsafe { i915_gem_object_gtt_prepare_pages(obj, st) };
        if ret == 0 {
            break;
        }
        unsafe { sg_free_table(st) };
        if max_segment <= crate::linux_config::PAGE_SIZE as u32 {
            break;
        }
        max_segment = crate::linux_config::PAGE_SIZE as u32;
    }

    if ret != 0 {
        unsafe { i915_gem_object_userptr_drop_ref(obj) };
        unsafe { crate::linux::memory::kfree(st) };
        return ret;
    }

    WARN_ON_ONCE!(unsafe { cache_coherent(obj) & I915_BO_CACHE_COHERENT_FOR_WRITE == 0 });
    if unsafe { i915_gem_object_can_bypass_llc(obj) } {
        unsafe { set_cache_dirty(obj, true) };
    }
    unsafe { __i915_gem_object_set_pages(obj, st) };
    0
}

// upstream: i915_gem_userptr.c i915_gem_userptr_put_pages()
unsafe extern "C" fn i915_gem_userptr_put_pages(obj: *mut DrmI915GemObject, pages: *mut SgTable) {
    if pages.is_null() {
        return;
    }

    unsafe { __i915_gem_object_release_shmem(obj, pages, true) };
    unsafe { i915_gem_gtt_finish_pages(obj, pages) };
    if unsafe { object_readonly(obj) } {
        unsafe { set_cache_dirty(obj, false) };
    }

    // The generic scatterlist owner supplies the exact sgt_iter page walk.
    for page in unsafe { crate::linux::scatterlist::sg_table_pages(pages) } {
        if unsafe { object_is_dirty(obj) }
            && unsafe { crate::linux::mmu_notifier::trylock_page(page) }
        {
            unsafe { set_page_dirty(page) };
            unsafe { unlock_page(page) };
        }
        unsafe { mark_page_accessed(page) };
    }

    unsafe { set_cache_dirty(obj, false) };
    unsafe { sg_free_table(pages) };
    unsafe { crate::linux::memory::kfree(pages) };
    unsafe { i915_gem_object_userptr_drop_ref(obj) };
}

// upstream: i915_gem_userptr.c i915_gem_object_userptr_unbind()
unsafe fn i915_gem_object_userptr_unbind(obj: *mut DrmI915GemObject) -> i32 {
    let err = unsafe { i915_gem_object_unbind(obj, I915_GEM_OBJECT_UNBIND_ACTIVE) };
    if err != 0 {
        return err;
    }
    if unsafe { object_has_pinned_pages(obj) } {
        return -crate::linux_config::EBUSY;
    }

    // assert_object_held() compiles away with LOCKDEP=n in this target.
    let pages = unsafe { __i915_gem_object_unset_pages(obj) };
    if !pages.is_null() && !crate::linux_config::IS_ERR(pages) {
        unsafe { i915_gem_userptr_put_pages(obj, pages) };
    }
    err
}

static I915_GEM_USERPTR_OPS: DrmI915GemObjectOps = DrmI915GemObjectOps {
    flags: I915_GEM_OBJECT_IS_SHRINKABLE | I915_GEM_OBJECT_NO_MMAP | I915_GEM_OBJECT_IS_PROXY,
    get_pages: Some(i915_gem_userptr_get_pages),
    put_pages: Some(i915_gem_userptr_put_pages),
    truncate: None,
    shrink: None,
    pread: Some(i915_gem_userptr_pread),
    pwrite: Some(i915_gem_userptr_pwrite),
    mmap_offset: None,
    unmap_virtual: None,
    dmabuf_export: Some(i915_gem_userptr_dmabuf_export),
    adjust_lru: None,
    delayed_free: None,
    migrate: None,
    release: Some(i915_gem_userptr_release),
    mmap_ops: core::ptr::null(),
    name: b"i915_gem_object_userptr\0".as_ptr().cast::<c_char>(),
};

// upstream: i915_gem_userptr.c i915_gem_object_userptr_submit_init()
pub unsafe fn i915_gem_object_userptr_submit_init(obj: *mut DrmI915GemObject) -> i32 {
    let base = unsafe { gem_base(obj) };
    let userptr = unsafe { userptr_record(obj) };
    let num_pages = unsafe { (*base).size >> PAGE_SHIFT } as c_ulong;
    let mm = crate::linux::mmu_notifier::current_mm();
    if unsafe { (*userptr).notifier.mm != mm } {
        return -crate::linux_config::EFAULT;
    }

    let notifier = unsafe { core::ptr::addr_of_mut!((*userptr).notifier) };
    let notifier_seq = unsafe { mmu_interval_read_begin(notifier) };
    let ret = unsafe { object_lock_interruptible(obj) };
    if ret != 0 {
        return ret;
    }

    if notifier_seq == unsafe { (*userptr).notifier_seq } && !unsafe { (*userptr).pvec }.is_null() {
        unsafe { object_unlock(obj) };
        return 0;
    }

    let ret = unsafe { i915_gem_object_userptr_unbind(obj) };
    unsafe { object_unlock(obj) };
    if ret != 0 {
        return ret;
    }

    let mut pvec = crate::linux::memory::kvmalloc_objs::<*mut Page, u64>(num_pages);
    if pvec.is_null() {
        return -crate::linux_config::ENOMEM;
    }

    let mut gup_flags = 0;
    if !unsafe { object_readonly(obj) } {
        gup_flags |= FOLL_WRITE;
    }

    let mut pinned = 0u64;
    let mut ret = 0;
    while pinned < num_pages {
        let address = unsafe { (*userptr).ptr as c_ulong }
            .wrapping_add(pinned.wrapping_mul(crate::linux_config::PAGE_SIZE as c_ulong));
        ret = unsafe {
            pin_user_pages_fast(
                address,
                num_pages.wrapping_sub(pinned) as i32,
                gup_flags,
                pvec.add(pinned as usize),
            )
        };
        if ret < 0 {
            break;
        }
        pinned = pinned.wrapping_add(ret as u64);
    }
    if ret < 0 {
        unsafe { unpin_user_pages(pvec, pinned as c_ulong) };
        unsafe { kvfree(pvec.cast::<c_void>()) };
        return ret;
    }

    let ret = unsafe { object_lock_interruptible(obj) };
    if ret != 0 {
        unsafe { unpin_user_pages(pvec, pinned as c_ulong) };
        unsafe { kvfree(pvec.cast::<c_void>()) };
        return ret;
    }

    let page_ref = unsafe { (*userptr).page_ref };
    let seq = if page_ref == 0 {
        notifier_seq
    } else {
        unsafe { (*userptr).notifier_seq }
    };
    if mmu_interval_read_retry(notifier, seq) {
        unsafe { object_unlock(obj) };
        unsafe { unpin_user_pages(pvec, pinned as c_ulong) };
        unsafe { kvfree(pvec.cast::<c_void>()) };
        return -crate::linux_config::EAGAIN;
    }

    let old_refs = unsafe { (*userptr).page_ref };
    unsafe { (*userptr).page_ref = old_refs.wrapping_add(1) };
    let mut ret = 0;
    if old_refs == 0 {
        unsafe {
            (*userptr).pvec = pvec;
            (*userptr).notifier_seq = notifier_seq;
        }
        pvec = core::ptr::null_mut();
        ret = unsafe { ____i915_gem_object_get_pages(obj) };
    }
    unsafe { (*userptr).page_ref = (*userptr).page_ref.wrapping_sub(1) };
    unsafe { object_unlock(obj) };

    if !pvec.is_null() {
        unsafe { unpin_user_pages(pvec, pinned as c_ulong) };
        unsafe { kvfree(pvec.cast::<c_void>()) };
    }
    ret
}

// upstream: i915_gem_userptr.c i915_gem_object_userptr_submit_done()
pub unsafe fn i915_gem_object_userptr_submit_done(obj: *mut DrmI915GemObject) -> i32 {
    let userptr = unsafe { userptr_record(obj) };
    if mmu_interval_read_retry(
        unsafe { core::ptr::addr_of!((*userptr).notifier) },
        unsafe { (*userptr).notifier_seq },
    ) {
        return -crate::linux_config::EAGAIN;
    }
    0
}

// upstream: i915_gem_userptr.c i915_gem_object_userptr_validate()
pub unsafe fn i915_gem_object_userptr_validate(obj: *mut DrmI915GemObject) -> i32 {
    let err = unsafe { i915_gem_object_userptr_submit_init(obj) };
    if err != 0 {
        return err;
    }

    let err = unsafe { object_lock_interruptible(obj) };
    if err != 0 {
        return err;
    }

    // Validation only: a notifier collision need not be retried here.
    let err = unsafe { crate::i915_gem_object_api_upstream::i915_gem_object_pin_pages(obj) };
    if err == 0 {
        unsafe { crate::i915_gem_object_api_upstream::i915_gem_object_unpin_pages(obj) };
    }
    unsafe { object_unlock(obj) };
    err
}

// upstream: i915_gem_userptr.c i915_gem_userptr_release()
unsafe extern "C" fn i915_gem_userptr_release(obj: *mut DrmI915GemObject) {
    GEM_WARN_ON!(unsafe { userptr_page_ref(obj) != 0 });
    let userptr = unsafe { userptr_record(obj) };
    if unsafe { (*userptr).notifier.mm.is_null() } {
        return;
    }
    unsafe { mmu_interval_notifier_remove(core::ptr::addr_of_mut!((*userptr).notifier)) };
    unsafe { (*userptr).notifier.mm = core::ptr::null_mut() };
}

// upstream: i915_gem_userptr.c i915_gem_userptr_dmabuf_export()
unsafe extern "C" fn i915_gem_userptr_dmabuf_export(obj: *mut DrmI915GemObject) -> i32 {
    drm_dbg!(
        unsafe { (*gem_base(obj)).dev },
        "Exporting userptr no longer allowed\n"
    );
    -crate::linux_config::EINVAL
}

// upstream: i915_gem_userptr.c i915_gem_userptr_pwrite()
unsafe extern "C" fn i915_gem_userptr_pwrite(
    obj: *mut DrmI915GemObject,
    _args: *const DrmI915GemPwrite,
) -> i32 {
    drm_dbg!(
        unsafe { (*gem_base(obj)).dev },
        "pwrite to userptr no longer allowed\n"
    );
    -crate::linux_config::EINVAL
}

// upstream: i915_gem_userptr.c i915_gem_userptr_pread()
unsafe extern "C" fn i915_gem_userptr_pread(
    obj: *mut DrmI915GemObject,
    _args: *const DrmI915GemPread,
) -> i32 {
    drm_dbg!(
        unsafe { (*gem_base(obj)).dev },
        "pread from userptr no longer allowed\n"
    );
    -crate::linux_config::EINVAL
}

// upstream: i915_gem_userptr.c probe_range()
unsafe fn probe_range(mm: *mut MmStruct, mut addr: c_ulong, len: c_ulong) -> i32 {
    let end = addr.wrapping_add(len);
    let mut vmi = crate::linux::mmu_notifier::VmaIterator::new(mm, addr);
    let mut vma = core::ptr::null_mut::<crate::linux::mm::VmAreaStruct>();

    unsafe { crate::linux::mmu_notifier::mmap_read_lock(mm) };
    loop {
        vma = unsafe { crate::linux::mmu_notifier::vma_find(&mut vmi, end) };
        if vma.is_null() {
            break;
        }
        if unsafe { (*vma).vm_start > addr } {
            break;
        }
        if unsafe {
            (*vma).vm_flags
                & (crate::linux::mm::VM_PFNMAP | crate::linux::mm::VM_MIXEDMAP)
                != 0
        } {
            break;
        }
        addr = unsafe { (*vma).vm_end };
    }
    unsafe { crate::linux::mmu_notifier::mmap_read_unlock(mm) };

    if !vma.is_null() || addr < end {
        -crate::linux_config::EFAULT
    } else {
        0
    }
}

/// `to_intel_bo()`-equivalent source base access for the userptr object.
unsafe fn set_userptr_readonly(obj: *mut DrmI915GemObject) {
    unsafe { (*obj).flags |= I915_BO_READONLY };
}

// upstream: i915_gem_userptr.c i915_gem_userptr_ioctl()
pub unsafe fn i915_gem_userptr_ioctl(
    dev: *mut c_void,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    let i915 = crate::linux::i915::to_i915(dev);
    let args = data.cast::<DrmI915GemUserptr>();

    if !unsafe { crate::linux::i915::HAS_LLC(i915) }
        && !unsafe { crate::linux::i915::HAS_SNOOP(i915) }
    {
        return -crate::linux_config::ENODEV;
    }
    let flags = unsafe { (*args).flags };
    if flags & !(I915_USERPTR_READ_ONLY | I915_USERPTR_UNSYNCHRONIZED | I915_USERPTR_PROBE) != 0 {
        return -crate::linux_config::EINVAL;
    }

    let user_ptr = unsafe { (*args).user_ptr };
    let user_size = unsafe { (*args).user_size };
    if user_size > usize::MAX as u64 {
        return -crate::linux_config::E2BIG;
    }
    if user_size == 0 {
        return -crate::linux_config::EINVAL;
    }
    if (user_ptr | user_size) & (crate::linux_config::PAGE_SIZE as u64 - 1) != 0 {
        return -crate::linux_config::EINVAL;
    }
    if !unsafe { crate::i915_gem_core_upstream::access_ok(user_ptr as *const c_void, user_size) } {
        return -crate::linux_config::EFAULT;
    }
    if flags & I915_USERPTR_UNSYNCHRONIZED != 0 {
        return -crate::linux_config::ENODEV;
    }

    if flags & I915_USERPTR_READ_ONLY != 0 {
        let gt = unsafe { crate::linux::i915::to_gt(i915) };
        let vm = unsafe { (*gt).vm };
        if unsafe { (*vm).vm_flags & (1 << 2) == 0 } {
            return -crate::linux_config::ENODEV;
        }
    }

    if flags & I915_USERPTR_PROBE != 0 {
        let mm = crate::linux::mmu_notifier::current_mm();
        let ret = unsafe { probe_range(mm, user_ptr as c_ulong, user_size as c_ulong) };
        if ret != 0 {
            return ret;
        }
    }

    {
        let obj = unsafe { i915_gem_object_alloc() };
        if obj.is_null() {
            return -crate::linux_config::ENOMEM;
        }
        let base = unsafe { gem_base(obj) };
        unsafe { drm_gem_private_object_init(dev, base, user_size as usize) };
        unsafe {
            i915_gem_object_init(
                obj,
                core::ptr::addr_of!(I915_GEM_USERPTR_OPS),
                core::ptr::addr_of_mut!(I915_GEM_USERPTR_LOCK_CLASS),
                I915_BO_ALLOC_USER as u32,
            )
        };
        unsafe { (*obj).mem_flags = I915_BO_FLAG_STRUCT_PAGE };
        unsafe {
            (*obj).read_domains = I915_GEM_DOMAIN_CPU as u16;
            (*obj).write_domain = I915_GEM_DOMAIN_CPU as u16;
            i915_gem_object_set_cache_coherency(
                obj,
                crate::i915_gem_object_types_upstream::I915CacheLevel::I915_CACHE_LLC as u32,
            );
        }
        let userptr = unsafe { userptr_record(obj) };
        unsafe {
            (*userptr).ptr = user_ptr as usize;
            (*userptr).notifier_seq = c_ulong::MAX;
        }
        if flags & I915_USERPTR_READ_ONLY != 0 {
            unsafe { set_userptr_readonly(obj) };
        }

        let ret = unsafe { i915_gem_userptr_init__mmu_notifier(obj) };
        let mut handle = 0;
        let ret = if ret == 0 {
            unsafe { drm_gem_handle_create(file, base, &mut handle) }
        } else {
            ret
        };

        // The handle owns the surviving reference on success.
        unsafe { object_put(obj) };
        if ret != 0 {
            return ret;
        }
        unsafe { (*args).handle = handle };
        0
    }
}
