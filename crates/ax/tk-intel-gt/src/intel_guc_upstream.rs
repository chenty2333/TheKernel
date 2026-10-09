// SPDX-License-Identifier: MIT
// Copyright © 2008-2026 Intel Corporation.
// Source integration from Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc.c.

use crate::{
    intel_workarounds_types_upstream::I915RegT,
    i915_gem_lmem_upstream::i915_gem_object_is_lmem,
    intel_guc_ct_types_upstream::IntelGucCt,
    intel_guc_types_upstream::IntelGuc,
    intel_gt_api_upstream::guc_to_gt,
    intel_uncore_types_upstream::intel_uncore_write_fw,
    linux::primitives::wmb,
};

// upstream: intel_guc.c intel_guc_write_barrier()
pub unsafe fn intel_guc_write_barrier(guc: &IntelGuc) {
    let guc_ptr = (guc as *const IntelGuc).cast_mut();
    let gt = unsafe { guc_to_gt(guc_ptr) };
    let ct: *const IntelGucCt = unsafe { &guc.ct };
    if unsafe { i915_gem_object_is_lmem((*(*ct).vma).obj) } {
        // Firmware-forcewake MMIO writes are only legal when the GuC send
        // registers do not own any forcewake domains.
        GEM_BUG_ON!(unsafe { guc.send_regs.fw_domains } != 0);
        unsafe {
            intel_uncore_write_fw(
                (*gt).uncore,
                I915RegT { reg: 0x190240 },
                0,
            );
        }
    } else {
        // Shared-memory communication needs the source's write barrier.
        wmb();
    }
}
