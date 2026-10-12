// SPDX-License-Identifier: MIT
// Copyright © 2024 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_display_conversion.c

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;
use tk_drm_upstream::drm_device_types::DrmDevice;

// upstream: intel_display_conversion.c __drm_to_display()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_to_display(drm: *mut DrmDevice) -> *mut IntelDisplay {
    if drm.is_null() {
        return core::ptr::null_mut();
    }
    // In i915, drm is embedded at the start of drm_i915_private.
    // display pointer is at offset 1560 in drm_i915_private.
    let i915 = crate::linux::i915::to_i915(drm.cast());
    if i915.is_null() {
        return core::ptr::null_mut();
    }
    (*i915).display.cast()
}
