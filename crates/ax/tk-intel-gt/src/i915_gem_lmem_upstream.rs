// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Source-faithful Linux 7.2.3 drivers/gpu/drm/i915/gem/i915_gem_lmem.c.

use crate::{
    i915_gem_object_api_upstream::i915_gem_object_put,
    i915_gem_object_header_upstream::{i915_gem_object_flush_map, i915_gem_object_is_contiguous},
    i915_gem_object_types_upstream::{DrmI915GemObject, I915_BO_ALLOC_CONTIGUOUS, I915_MAP_WC},
    i915_gem_pages_upstream::{__i915_gem_object_get_dma_address, __i915_gem_object_release_map, i915_gem_object_pin_map_unlocked},
    i915_gem_region_upstream::i915_gem_object_create_region,
    linux::gem_memory::{intel_memory_type_is_local, IntelRegionId, INTEL_REGION_LMEM_0},
    linux_config::{IS_ERR, PAGE_SIZE},
    linux_i915_private::DrmI915Private,
    linux::primitives::round_up,
    linux_macros::read_once,
};

// upstream: i915_gem_lmem.c i915_gem_object_lmem_io_map()
pub unsafe fn i915_gem_object_lmem_io_map(obj: *mut DrmI915GemObject, n: usize, size: usize) -> *mut core::ffi::c_void {
    GEM_BUG_ON!(!unsafe { i915_gem_object_is_contiguous(obj) });
    let mut offset = unsafe { __i915_gem_object_get_dma_address(obj, n as u64) };
    offset -= unsafe { (*(*obj).mm.region).region.start };
    unsafe { crate::i915_gem_core_upstream::io_mapping_map_wc(core::ptr::addr_of_mut!((*(*obj).mm.region).iomap).cast(), offset as i64, size) }
}

/// Whether an object is currently resident in device-local memory. Migratable
/// objects are meaningful here only while their object reservation is held,
/// exactly as in the upstream contract.
pub unsafe fn i915_gem_object_is_lmem(obj: *mut DrmI915GemObject) -> bool {
    assert!(!obj.is_null());
    let region = read_once(core::ptr::addr_of!((*obj).mm.region));
    !region.is_null() && unsafe { intel_memory_type_is_local((*region).r#type) }
}

// upstream: i915_gem_lmem.c __i915_gem_object_create_lmem_with_ps()
pub unsafe fn __i915_gem_object_create_lmem_with_ps(i915: *mut DrmI915Private, size: u64, page_size: u64, flags: u32) -> *mut DrmI915GemObject {
    let mem = unsafe { (*i915).mm.regions[INTEL_REGION_LMEM_0 as usize] };
    unsafe { i915_gem_object_create_region(mem, size, page_size, flags) }
}

// upstream: i915_gem_lmem.c i915_gem_object_create_lmem()
pub unsafe fn i915_gem_object_create_lmem(i915: *mut DrmI915Private, size: u64, flags: u32) -> *mut DrmI915GemObject {
    unsafe { __i915_gem_object_create_lmem_with_ps(i915, size, 0, flags) }
}

// upstream: i915_gem_lmem.c i915_gem_object_create_lmem_from_data()
pub unsafe fn i915_gem_object_create_lmem_from_data(i915: *mut DrmI915Private, data: *const core::ffi::c_void, size: usize) -> *mut DrmI915GemObject {
    let obj = unsafe { i915_gem_object_create_lmem(i915, round_up(size as u64, PAGE_SIZE as u64), I915_BO_ALLOC_CONTIGUOUS as u32) };
    if IS_ERR(obj) { return obj; }
    let map = unsafe { i915_gem_object_pin_map_unlocked(obj, I915_MAP_WC) };
    if IS_ERR(map) {
        unsafe { i915_gem_object_put(obj) };
        return map.cast::<DrmI915GemObject>();
    }
    unsafe { core::ptr::copy_nonoverlapping(data.cast::<u8>(), map.cast::<u8>(), size) };
    unsafe { i915_gem_object_flush_map(obj) };
    unsafe { __i915_gem_object_release_map(obj) };
    obj
}

