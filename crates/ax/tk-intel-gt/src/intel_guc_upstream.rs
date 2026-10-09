// SPDX-License-Identifier: MIT
// Copyright © 2008-2026 Intel Corporation.
// Source integration from Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc.c.

use crate::{
    guc_ads,
    intel_workarounds_types_upstream::I915RegT,
    i915_gem_lmem_upstream::i915_gem_object_is_lmem,
    intel_guc_ct_types_upstream::IntelGucCt,
    intel_guc_types_upstream::IntelGuc,
    intel_engine_types_upstream::IntelEngineCs,
    intel_gt_api_upstream::gt_to_guc,
    i915_vma_types_upstream::I915Vma,
    i915_vma_api_upstream::i915_ggtt_pin_bias,
    intel_gt_api_upstream::guc_to_gt,
    intel_uncore_types_upstream::intel_uncore_write_fw,
    linux::{i915::i915_ggtt_offset, primitives::wmb},
};
use core::ffi::c_void;
use crate::linux::iosys_map::{IosysMap, IosysMapAddr};

// upstream: intel_guc_ads.c intel_guc_engine_usage_offset().
pub unsafe fn intel_guc_engine_usage_offset(guc: *mut IntelGuc) -> u32 {
    let base = unsafe { intel_guc_ggtt_offset(guc, (*guc).ads_vma) };
    guc_ads::engine_usage_offset(base).expect("valid page-aligned GuC ADS GGTT address")
}

// upstream: intel_guc_ads.c intel_guc_engine_usage_record_map().
pub unsafe fn intel_guc_engine_usage_record_map(engine: &IntelEngineCs) -> IosysMap {
    let guc = unsafe { gt_to_guc(engine.gt) };
    let guc_class = guc_ads::engine_class_to_guc_class(engine.class)
        .expect("registered engine class has a GuC class");
    let mask = engine.logical_mask;
    assert_ne!(mask, 0, "engine logical mask must be nonzero");
    let logical_index = (u32::BITS - 1 - mask.leading_zeros()) as u8;
    let offset = guc_ads::engine_usage_record_offset(guc_class, logical_index)
        .expect("engine usage record index is in the ADS array");
    let map = unsafe { &(*guc).ads_map };
    let mut result = IosysMap {
        addr: IosysMapAddr { vaddr: core::ptr::null_mut() },
        is_iomem: map.is_iomem,
    };
    unsafe {
        if map.is_iomem {
            let base = map.addr.vaddr_iomem.cast::<u8>();
            result.addr.vaddr_iomem = base.wrapping_add(offset).cast::<c_void>();
        } else {
            let base = map.addr.vaddr.cast::<u8>();
            result.addr.vaddr = base.wrapping_add(offset).cast::<c_void>();
        }
    }
    result
}

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

// upstream: intel_guc.h intel_guc_ggtt_offset()
pub unsafe fn intel_guc_ggtt_offset(_guc: *mut IntelGuc, vma: *const I915Vma) -> u32 {
    const GUC_GGTT_TOP: u64 = 0xFEE0_0000;
    let offset = unsafe { i915_ggtt_offset(vma) };
    GEM_BUG_ON!(offset < unsafe { i915_ggtt_pin_bias(vma) });
    let start = offset as u64;
    let size = unsafe { (*vma).size };
    GEM_BUG_ON!(start >= GUC_GGTT_TOP || size > GUC_GGTT_TOP - start);
    offset
}
