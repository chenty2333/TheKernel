// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
// Linux v7.2.3 scatterlist helpers used by i915's GEM memory paths.

use core::ffi::c_void;

use crate::{
    i915_gem_object_types_upstream::Page,
    i915_gem_pages_upstream::{sg_next, sg_page, Scatterlist},
    intel_context_upstream::SgTable,
    linux_config::PAGE_SHIFT,
};

#[repr(C)]
struct SgTableLayout {
    sgl: *mut Scatterlist,
    nents: u32,
    orig_nents: u32,
}

const _: [(); 16] = [(); core::mem::size_of::<SgTableLayout>()];

/// Iterator corresponding to a page walk of the mapped segments in an sg_table.
pub struct SgTablePages {
    sg: *mut Scatterlist,
    remaining: u32,
}

impl Iterator for SgTablePages {
    type Item = *mut Page;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 || self.sg.is_null() {
            return None;
        }
        let current = self.sg;
        unsafe {
            self.sg = sg_next(current);
            self.remaining -= 1;
            Some(sg_page(current))
        }
    }
}

/// Walk each DMA-mapped sg segment's starting page.
pub unsafe fn sg_table_pages(table: *mut SgTable) -> SgTablePages {
    let layout = unsafe { &*table.cast::<SgTableLayout>() };
    SgTablePages {
        sg: layout.sgl,
        remaining: layout.nents,
    }
}

unsafe extern "C" {
    fn dma_max_mapping_size(dev: *mut c_void) -> usize;
}

/// `i915_sg_segment_size()` from `i915_scatterlist.h`; CONFIG_XEN=n in the
/// wt-dev x86_64 target, so the Xen PV page-sized cap is compiled out.
pub unsafe fn i915_sg_segment_size(dev: *mut c_void) -> u32 {
    let max = usize::min(u32::MAX as usize, unsafe { dma_max_mapping_size(dev) });
    (((max >> PAGE_SHIFT) << PAGE_SHIFT) as u32)
}
