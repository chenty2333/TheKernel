// SPDX-License-Identifier: MIT
// Copyright © 2025 Intel Corporation
// Source: drivers/gpu/drm/i915/i915_panic.c
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ptr, ffi::c_void};

use crate::{
    i915_gem_pages_upstream::{
        DrmScanoutBuffer, i915_gem_object_alloc_panic, i915_gem_object_panic_finish,
        i915_gem_object_panic_setup,
    },
    intel_display_types_upstream::IntelFramebuffer,
    intel_fb_upstream::intel_fb_bo,
    intel_parent_upstream::IntelDisplayPanicInterface,
};

// upstream: i915_panic.c intel_panic_alloc()
unsafe extern "C" fn intel_panic_alloc() -> *mut c_void {
    unsafe { i915_gem_object_alloc_panic().cast() }
}

// upstream: i915_panic.c intel_panic_setup()
unsafe extern "C" fn intel_panic_setup(panic: *mut c_void, sb: *mut c_void) -> i32 {
    unsafe {
        let sb = sb.cast::<DrmScanoutBuffer>();
        let fb = (*sb).private.cast::<IntelFramebuffer>();
        let obj = intel_fb_bo(ptr::addr_of!((*fb).base));

        // The C `bool panic_tiling` argument receives the `fb->panic_tiling`
        // function pointer, i.e. non-NULL means true.
        i915_gem_object_panic_setup(
            panic.cast(),
            sb,
            obj.cast(),
            (*fb).panic_tiling.is_some(),
        )
    }
}

// upstream: i915_panic.c intel_panic_finish()
unsafe extern "C" fn intel_panic_finish(panic: *mut c_void) {
    unsafe { i915_gem_object_panic_finish(panic.cast()) }
}

/// i915-owned panic-console callbacks exported to the display parent.
#[unsafe(no_mangle)]
pub static i915_display_panic_interface: IntelDisplayPanicInterface = IntelDisplayPanicInterface {
    alloc: Some(intel_panic_alloc),
    setup: Some(intel_panic_setup),
    finish: Some(intel_panic_finish),
};
