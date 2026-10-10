// SPDX-License-Identifier: MIT
// Copyright © 2021 Intel Corporation
//! Linux 7.2.3 intel_region_ttm.c device-level entry points, in source order.
//!
//! Translated: intel_region_ttm_device_init() and intel_region_ttm_device_fini(),
//! which every platform's probe calls. The TTM device layer they drive is in
//! `linux::ttm`.
//!
//! Not translated: intel_region_to_ttm_type(), intel_region_ttm_init/fini(),
//! intel_region_ttm_resource_to_rsgt/alloc/free(). Their only callers are
//! gt/intel_region_lmem.c (local memory) and selftests/mock_region.c, and
//! LMEM/TTM regions are outside TheKernel's supported hardware. The TTM BO
//! callbacks that i915 installs (gem/i915_gem_ttm.c, `i915_ttm_bo_driver`) are
//! defined below as fail-closed hooks: ttm_device_init() only stores the table,
//! and the callbacks run only from TTM BO paths, which are not reachable here.
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::ffi::{c_int, c_void};

use crate::{
    linux::{
        i915_private::DrmI915Private,
        ttm::{TtmDeviceFuncs, ttm_device_fini, ttm_device_init},
    },
};

/// One fail-closed TTM BO callback for `i915_ttm_bo_driver`.
macro_rules! lmem_only_hook {
    ($name:ident, $callback:literal) => {
        // upstream: gem/i915_gem_ttm.c $callback
        unsafe extern "C" fn $name() {
            panic!(concat!(
                "i915 TTM BO callback ",
                $callback,
                " serves LMEM/TTM objects, which TheKernel does not support"
            ))
        }
    };
}

lmem_only_hook!(ttm_tt_create, "i915_ttm_tt_create()");
lmem_only_hook!(ttm_tt_populate, "i915_ttm_tt_populate()");
lmem_only_hook!(ttm_tt_unpopulate, "i915_ttm_tt_unpopulate()");
lmem_only_hook!(ttm_tt_destroy, "i915_ttm_tt_destroy()");
lmem_only_hook!(eviction_valuable, "i915_ttm_eviction_valuable()");
lmem_only_hook!(evict_flags, "i915_ttm_evict_flags()");
lmem_only_hook!(move_bo, "i915_ttm_move()");
lmem_only_hook!(swap_notify, "i915_ttm_swap_notify()");
lmem_only_hook!(delete_mem_notify, "i915_ttm_delete_mem_notify()");
lmem_only_hook!(io_mem_reserve, "i915_ttm_io_mem_reserve()");
lmem_only_hook!(io_mem_pfn, "i915_ttm_io_mem_pfn()");
lmem_only_hook!(access_memory, "i915_ttm_access_memory()");

/// `static struct ttm_device_funcs i915_ttm_bo_driver`. io_mem_free and
/// release_notify are NULL in the upstream table, and stay None here.
static I915_TTM_BO_DRIVER: TtmDeviceFuncs = TtmDeviceFuncs {
    ttm_tt_create: Some(ttm_tt_create),
    ttm_tt_populate: Some(ttm_tt_populate),
    ttm_tt_unpopulate: Some(ttm_tt_unpopulate),
    ttm_tt_destroy: Some(ttm_tt_destroy),
    eviction_valuable: Some(eviction_valuable),
    evict_flags: Some(evict_flags),
    move_: Some(move_bo),
    delete_mem_notify: Some(delete_mem_notify),
    swap_notify: Some(swap_notify),
    io_mem_reserve: Some(io_mem_reserve),
    io_mem_free: None,
    io_mem_pfn: Some(io_mem_pfn),
    access_memory: Some(access_memory),
    release_notify: None,
};

/// Linux `i915_ttm_driver()` (gem/i915_gem_ttm.c).
pub fn i915_ttm_driver() -> *const TtmDeviceFuncs {
    &I915_TTM_BO_DRIVER
}

// upstream: intel_region_ttm.c intel_region_ttm_device_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_region_ttm_device_init(dev_priv: *mut DrmI915Private) -> c_int {
    let drm = core::ptr::addr_of_mut!((*dev_priv).drm);
    ttm_device_init(
        core::ptr::addr_of_mut!((*dev_priv).bdev),
        i915_ttm_driver(),
        (*drm).dev,
        (*(*drm).anon_inode).i_mapping,
        (*drm).vma_offset_manager.cast::<c_void>(),
        0,
    )
}

// upstream: intel_region_ttm.c intel_region_ttm_device_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_region_ttm_device_fini(dev_priv: *mut DrmI915Private) {
    ttm_device_fini(core::ptr::addr_of_mut!((*dev_priv).bdev));
}
