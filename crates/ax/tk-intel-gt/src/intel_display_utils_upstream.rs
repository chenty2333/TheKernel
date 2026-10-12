// SPDX-License-Identifier: MIT
// Copyright © 2025 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_display_utils.c

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;

use crate::intel_display_core_types_upstream::IntelDisplay;

unsafe extern "C" {
    fn hypervisor_is_type(type_: u32) -> bool;
    fn device_iommu_mapped(dev: *mut c_void) -> bool;
}

const X86_HYPER_NATIVE: u32 = 0;

// upstream: intel_display_utils.c intel_display_run_as_guest()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_run_as_guest(_display: *mut IntelDisplay) -> bool {
    !hypervisor_is_type(X86_HYPER_NATIVE)
}

// upstream: intel_display_utils.c intel_display_vtd_active()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_vtd_active(display: *mut IntelDisplay) -> bool {
    if display.is_null() || (*display).drm.is_null() {
        return false;
    }
    let dev = (*(*display).drm).dev;
    if !dev.is_null() && device_iommu_mapped(dev.cast()) {
        return true;
    }

    // Running as a guest, we assume the host is enforcing VT-d
    intel_display_run_as_guest(display)
}
