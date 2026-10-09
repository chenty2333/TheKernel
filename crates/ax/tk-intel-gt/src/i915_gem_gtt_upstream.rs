// SPDX-License-Identifier: MIT
// Copyright © 2014-2016 Intel Corporation.
//! Linux 7.2.3 i915_gem_gtt.c DMA page prepare/finish, in source order.
#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]
use crate::{
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_gem_shrinker_upstream::i915_gem_shrink,
    intel_context_upstream::SgTable,
    linux::{
        dma::{dma_map_sg_attrs, dma_unmap_sg_attrs},
        gem::drm_device_device,
        i915::to_i915,
    },
    linux_config::ENOSPC,
};
// upstream: i915_gem_gtt.c i915_gem_gtt_prepare_pages()
pub unsafe fn i915_gem_gtt_prepare_pages(obj: *mut DrmI915GemObject, pages: *mut SgTable) -> i32 {
    loop {
        if dma_map_sg_attrs(
            drm_device_device((&(*obj).base.base).dev),
            (*pages).sgl,
            (*pages).nents,
            0,
            (1 << 5) | (1 << 4) | (1 << 8),
        ) != 0
        {
            return 0;
        }
        GEM_BUG_ON!((*obj).mm.pages == pages);
        if i915_gem_shrink(
            core::ptr::null_mut(),
            to_i915((&(*obj).base.base).dev),
            ((&(*obj).base.base).size >> 12) as u64,
            core::ptr::null_mut(),
            3,
        ) == 0
        {
            break;
        }
    }
    -ENOSPC
}
// upstream: i915_gem_gtt.c i915_gem_gtt_finish_pages()
pub unsafe fn i915_gem_gtt_finish_pages(obj: *mut DrmI915GemObject, pages: *mut SgTable) {
    let i915 = to_i915((&(*obj).base.base).dev);
    let ggtt = (*crate::linux::i915::to_gt(i915)).ggtt;
    if (*ggtt).do_idle_maps {
        axtask::sleep(core::time::Duration::from_micros(100));
    }
    dma_unmap_sg_attrs(
        drm_device_device((&(*obj).base.base).dev),
        (*pages).sgl,
        (*pages).nents,
        0,
        0,
    );
}
