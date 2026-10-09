// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Linux v7.2.3 drivers/gpu/drm/i915/gem/i915_gem_region.c translation.

use core::ffi::c_int;

use crate::{
    i915_gem_object_api_upstream::{i915_gem_object_lock, i915_gem_object_put},
    i915_gem_object_header_upstream::i915_gem_object_size_2big,
    i915_gem_object_types_upstream::{intel_bo_to_drm_bo, DrmI915GemObject, I915_BO_ALLOC_CONTIGUOUS, I915_BO_ALLOC_CPU_CLEAR, I915_BO_ALLOC_FLAGS, I915_BO_ALLOC_GPU_ONLY, I915_BO_ALLOC_PM_EARLY},
    i915_gem_object_upstream::{i915_gem_object_alloc, i915_gem_object_free},
    i915_gem_ww_upstream::{I915GemWwCtx, i915_gem_ww_ctx_backoff, i915_gem_ww_ctx_fini, i915_gem_ww_ctx_init},
    intel_engine_cs_upstream::ListHead,
    linux::gem_memory::IntelMemoryRegion,
    linux::i915::to_gt,
    linux_config::{E2BIG, EINVAL, ENODEV, ENOMEM, ENOSPC, PAGE_SIZE},
    linux_list::{INIT_LIST_HEAD, list_add, list_del, list_move_tail},
    linux_memory::kref_get_unless_zero,
    linux_mutex::{mutex_lock, mutex_unlock},
    linux::primitives::round_up,
};

const I915_BO_INVALID_OFFSET: u64 = u64::MAX;
const I915_GTT_MIN_ALIGNMENT: u64 = PAGE_SIZE as u64;

#[repr(C)]
pub struct IntelMemoryRegionOps {
    pub init: Option<unsafe extern "C" fn(*mut IntelMemoryRegion) -> c_int>,
    pub release: Option<unsafe extern "C" fn(*mut IntelMemoryRegion) -> c_int>,
    pub init_object: Option<unsafe extern "C" fn(*mut IntelMemoryRegion, *mut DrmI915GemObject, u64, u64, u64, u32) -> c_int>,
}

#[repr(C)]
pub struct I915GemApplyToRegionOps {
    pub process_obj: Option<unsafe extern "C" fn(*mut I915GemApplyToRegion, *mut DrmI915GemObject) -> c_int>,
}

#[repr(C)]
pub struct I915GemApplyToRegion {
    pub ops: *const I915GemApplyToRegionOps,
    pub ww: *mut I915GemWwCtx,
    pub interruptible: u32,
}

// upstream: i915_gem_region.c i915_gem_object_init_memory_region()
pub unsafe fn i915_gem_object_init_memory_region(obj: *mut DrmI915GemObject, mem: *mut IntelMemoryRegion) {
    unsafe {
        (*obj).mm.region = mem;
        mutex_lock(&mut (*mem).objects.lock);
        list_add(&mut (*obj).mm.region_link, &mut (*mem).objects.list);
        mutex_unlock(&mut (*mem).objects.lock);
    }
}

// upstream: i915_gem_region.c i915_gem_object_release_memory_region()
pub unsafe fn i915_gem_object_release_memory_region(obj: *mut DrmI915GemObject) {
    unsafe {
        let mem = (*obj).mm.region;
        mutex_lock(&mut (*mem).objects.lock);
        list_del(&mut (*obj).mm.region_link);
        mutex_unlock(&mut (*mem).objects.lock);
    }
}

unsafe fn __i915_gem_object_create_region(mem: *mut IntelMemoryRegion, offset: u64, mut size: u64, page_size: u64, flags: u32) -> *mut DrmI915GemObject {
    GEM_BUG_ON!(flags & !I915_BO_ALLOC_FLAGS as u32 != 0);
    if WARN_ON_ONCE!(flags & I915_BO_ALLOC_GPU_ONLY as u32 != 0 && flags & (I915_BO_ALLOC_CPU_CLEAR | I915_BO_ALLOC_PM_EARLY) as u32 != 0) {
        return ERR_PTR!(-EINVAL);
    }
    if mem.is_null() {
        return ERR_PTR!(-ENODEV);
    }

    let mut default_page_size = unsafe { (*mem).min_page_size };
    if page_size != 0 { default_page_size = page_size; }
    GEM_BUG_ON!(default_page_size > u32::MAX as u64);
    GEM_BUG_ON!(!default_page_size.is_power_of_two());
    GEM_BUG_ON!(default_page_size < PAGE_SIZE as u64);
    size = round_up(size, default_page_size);
    let mut flags = flags;
    if default_page_size == size { flags |= I915_BO_ALLOC_CONTIGUOUS as u32; }
    GEM_BUG_ON!(size == 0);
    GEM_BUG_ON!(size & (I915_GTT_MIN_ALIGNMENT - 1) != 0);
    if i915_gem_object_size_2big(size) { return ERR_PTR!(-E2BIG); }
    let obj = unsafe { i915_gem_object_alloc() };
    if obj.is_null() { return ERR_PTR!(-ENOMEM); }
    if default_page_size < unsafe { (*mem).min_page_size } { flags |= I915_BO_ALLOC_PM_EARLY as u32; }
    let ops = unsafe { (*mem).ops.cast::<IntelMemoryRegionOps>() };
    let ret = unsafe { ((*ops).init_object.unwrap())(mem, obj, offset, size, page_size, flags) };
    if ret != 0 {
        unsafe { i915_gem_object_free(obj) };
        return ERR_PTR!(ret);
    }
    trace_i915_gem_object_create(obj);
    obj
}

// upstream: i915_gem_region.c i915_gem_object_create_region()
pub unsafe fn i915_gem_object_create_region(mem: *mut IntelMemoryRegion, size: u64, page_size: u64, flags: u32) -> *mut DrmI915GemObject {
    unsafe { __i915_gem_object_create_region(mem, I915_BO_INVALID_OFFSET, size, page_size, flags) }
}

// upstream: i915_gem_region.c i915_gem_object_create_region_at()
pub unsafe fn i915_gem_object_create_region_at(mem: *mut IntelMemoryRegion, offset: u64, size: u64, flags: u32) -> *mut DrmI915GemObject {
    GEM_BUG_ON!(offset == I915_BO_INVALID_OFFSET);
    if GEM_WARN_ON!(offset % unsafe { (*mem).min_page_size } != 0 || size % unsafe { (*mem).min_page_size } != 0) { return ERR_PTR!(-EINVAL); }
    let region_size = unsafe { (*mem).region.end.wrapping_sub((*mem).region.start).wrapping_add(1) };
    if offset.checked_add(size).is_none_or(|end| end > region_size) { return ERR_PTR!(-EINVAL); }
    if flags & I915_BO_ALLOC_GPU_ONLY as u32 == 0 {
        let io_size = unsafe { (*mem).io.end.wrapping_sub((*mem).io.start).wrapping_add(1) };
        if offset + size > io_size && !unsafe { crate::intel_gtt_api_upstream::i915_ggtt_has_aperture((*to_gt((*mem).i915)).ggtt.cast()) } { return ERR_PTR!(-ENOSPC); }
    }
    unsafe { __i915_gem_object_create_region(mem, offset, size, 0, flags | I915_BO_ALLOC_CONTIGUOUS as u32) }
}

// upstream: i915_gem_region.c i915_gem_process_region()
pub unsafe fn i915_gem_process_region(mr: *mut IntelMemoryRegion, apply: *mut I915GemApplyToRegion) -> c_int {
    let ops = unsafe { (*apply).ops };
    let mut still_storage = core::mem::MaybeUninit::<ListHead>::uninit();
    unsafe { INIT_LIST_HEAD(still_storage.as_mut_ptr()) };
    let mut still_in_list = unsafe { still_storage.assume_init() };
    GEM_WARN_ON!(unsafe { !(*apply).ww.is_null() });
    let mut ret = 0;
    unsafe { mutex_lock(&mut (*mr).objects.lock) };
    loop {
        let obj = list_first_entry_or_null!(&mut (*mr).objects.list, DrmI915GemObject, mm.region_link);
        if obj.is_null() { break; }
        unsafe { list_move_tail(&mut (*obj).mm.region_link, &mut still_in_list) };
        if !unsafe { kref_get_unless_zero(&mut (*intel_bo_to_drm_bo(obj)).refcount) } { continue; }
        unsafe { mutex_unlock(&mut (*mr).objects.lock) };
        let mut ww = I915GemWwCtx::default();
        unsafe { (*apply).ww = &mut ww };
        unsafe { i915_gem_ww_ctx_init(&mut ww, (*apply).interruptible != 0) };
        ret = -crate::linux_config::EDEADLK;
        loop {
            ret = unsafe { i915_gem_object_lock(obj, (*apply).ww) };
            if ret == 0 && unsafe { (*obj).mm.region == mr } {
                ret = unsafe { ((*ops).process_obj.unwrap())(apply, obj) };
            }
            if ret != -crate::linux_config::EDEADLK { break; }
            ret = unsafe { i915_gem_ww_ctx_backoff(&mut ww) };
            if ret == 0 { ret = -crate::linux_config::EDEADLK; } else { break; }
        }
        unsafe { i915_gem_ww_ctx_fini(&mut ww) };
        unsafe { i915_gem_object_put(obj) };
        unsafe { mutex_lock(&mut (*mr).objects.lock) };
        if ret != 0 { break; }
    }
    unsafe { list_splice_tail(&mut still_in_list, &mut (*mr).objects.list) };
    unsafe { mutex_unlock(&mut (*mr).objects.lock) };
    ret
}

/// Linux `list_splice_tail()` linkage, keeping `list` itself intact.
unsafe fn list_splice_tail(list: *mut ListHead, head: *mut ListHead) {
    unsafe {
        if (*list).next == list { return; }
        let first = (*list).next;
        let last = (*list).prev;
        let prev = (*head).prev;
        (*prev).next = first;
        (*first).prev = prev;
        (*last).next = head;
        (*head).prev = last;
    }
}

// Imported from i915_gem_region.c / DRM and memory-region subsystem owners.
unsafe extern "C" {
    fn trace_i915_gem_object_create(obj: *mut DrmI915GemObject);
}

