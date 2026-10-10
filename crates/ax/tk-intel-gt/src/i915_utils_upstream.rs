// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//! Linux 7.2.3 drivers/gpu/drm/i915/i915_utils.c VT-d status helper.
use crate::linux_i915_private::DrmI915Private;
// upstream: i915_utils.c i915_vtd_active()
pub unsafe fn i915_vtd_active(i915: *mut DrmI915Private) -> bool {
    if crate::linux::dma::device_iommu_mapped(unsafe { (*i915).drm.dev }) {
        return true;
    }
    unsafe { core::arch::x86_64::__cpuid(1).ecx & (1 << 31) != 0 }
}

// upstream: i915_utils.c i915_direct_stolen_access()
//
// Wa_22018444074 applies only to MTL (`IS_METEORLAKE`), which TheKernel does not
// support. Its MMIO probe (`MTL_PCODE_STOLEN_ACCESS`) therefore fails closed:
// reaching it panics instead of silently taking the BAR path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_direct_stolen_access(i915: *mut DrmI915Private) -> bool {
    if unsafe { crate::linux::i915::IS_METEORLAKE(i915) } {
        panic!("i915_direct_stolen_access: MTL (Wa_22018444074) is not supported by TheKernel");
    }
    false
}
