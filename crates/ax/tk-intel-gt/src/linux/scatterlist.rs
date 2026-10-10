// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
// Linux v7.2.3 scatterlist helpers used by i915's GEM memory paths.

#![allow(unsafe_code)]

use alloc::vec::Vec;
use core::{
    ffi::{c_int, c_ulong, c_void},
    mem::size_of,
    ptr,
};

use crate::{
    i915_gem_object_types_upstream::Page,
    i915_gem_pages_upstream::{Scatterlist, sg_mark_end, sg_next, sg_page},
    intel_context_upstream::SgTable,
    linux::{
        dma::dma_map_sg_attrs,
        memory::{kfree, kmalloc},
        shmem::try_page_to_phys,
    },
    linux_config::{EINVAL, ENOMEM, PAGE_SHIFT, PAGE_SIZE},
};

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
    SgTablePages {
        sg: unsafe { (*table).sgl },
        remaining: unsafe { (*table).nents },
    }
}

/// Linux `dma_max_mapping_size()`: with VT-d translation and no swiotlb
/// bounce buffer on this target, no per-device mapping cap exists.
#[unsafe(export_name = "dma_max_mapping_size")]
pub extern "C" fn dma_max_mapping_size(_dev: *mut c_void) -> usize {
    usize::MAX
}

/// `i915_sg_segment_size()` from `i915_scatterlist.h`; CONFIG_XEN=n in the
/// wt-dev x86_64 target, so the Xen PV page-sized cap is compiled out.
pub unsafe fn i915_sg_segment_size(dev: *mut c_void) -> u32 {
    let max = usize::min(u32::MAX as usize, dma_max_mapping_size(dev));
    (((max >> PAGE_SHIFT) << PAGE_SHIFT) as u32)
}

/// Linux `sg_alloc_table(table, nents, gfp)`. The entries form one array,
/// terminated by the end marker. Linux chains arrays above one page of
/// entries; `sg_next`-based walkers see the same sequence either way.
///
/// # Safety
/// `table` must point to a writable `sg_table`.
#[unsafe(export_name = "sg_alloc_table")]
pub unsafe extern "C" fn sg_alloc_table(table: *mut SgTable, nents: u32, gfp_mask: u32) -> c_int {
    if table.is_null() || nents == 0 {
        return -EINVAL;
    }
    let Some(bytes) = (nents as usize).checked_mul(size_of::<Scatterlist>()) else {
        return -EINVAL;
    };
    let sgl = kmalloc(bytes, gfp_mask).cast::<Scatterlist>();
    if sgl.is_null() {
        return -ENOMEM;
    }
    unsafe {
        ptr::write_bytes(sgl.cast::<u8>(), 0, bytes);
        sg_mark_end(sgl.add(nents as usize - 1));
        (*table).sgl = sgl;
        (*table).nents = nents;
        (*table).orig_nents = nents;
    }
    0
}

/// Linux `sg_free_table(table)`: releases the entry array and clears the table.
///
/// # Safety
/// `table` must have been filled by one of the `sg_alloc_table*` calls.
#[unsafe(export_name = "sg_free_table")]
pub unsafe extern "C" fn sg_free_table(table: *mut SgTable) {
    if table.is_null() {
        return;
    }
    unsafe {
        if !(*table).sgl.is_null() {
            kfree((*table).sgl);
        }
        (*table).sgl = ptr::null_mut();
        (*table).nents = 0;
        (*table).orig_nents = 0;
    }
}

/// One merged scatterlist segment before the entry array is allocated.
struct Segment {
    page: *mut Page,
    offset: u32,
    length: u32,
    physical_end: usize,
}

/// Linux `sg_alloc_table_from_pages_segment()`: build a table covering the
/// byte range `[offset, offset + size)` of `n_pages` pages. Physically
/// contiguous neighbours merge into one segment, and no segment exceeds
/// `max_segment` bytes.
///
/// # Safety
/// `pages` must hold `n_pages` registered page identities.
#[unsafe(export_name = "sg_alloc_table_from_pages_segment")]
pub unsafe extern "C" fn sg_alloc_table_from_pages_segment(
    table: *mut SgTable,
    pages: *mut *mut Page,
    n_pages: u32,
    offset: u32,
    size: c_ulong,
    max_segment: u32,
    gfp_mask: u32,
) -> c_int {
    let page_size = PAGE_SIZE;
    if table.is_null()
        || pages.is_null()
        || n_pages == 0
        || max_segment == 0
        || size == 0
        || offset as usize >= page_size
    {
        return -EINVAL;
    }
    let Some(end) = (offset as usize).checked_add(size as usize) else {
        return -EINVAL;
    };
    if end > (n_pages as usize) * page_size {
        return -EINVAL;
    }

    let mut segments: Vec<Segment> = Vec::new();
    for index in 0..n_pages as usize {
        let page_start = index * page_size;
        if page_start >= end {
            break;
        }
        let lo = (offset as usize).max(page_start);
        let hi = end.min(page_start + page_size);
        if lo >= hi {
            continue;
        }
        let page = unsafe { pages.add(index).read() };
        if page.is_null() {
            return -EINVAL;
        }
        let Some(page_physical) = (unsafe { try_page_to_phys(page) }) else {
            return -EINVAL;
        };
        let mut in_page = lo - page_start;
        let mut remaining = hi - lo;
        while remaining > 0 {
            let physical = page_physical + in_page;
            let mergeable = segments.last().is_some_and(|last| {
                last.physical_end == physical && (last.length as usize) < max_segment as usize
            });
            if mergeable {
                let last = segments.last_mut().expect("mergeable segment exists");
                let room = max_segment as usize - last.length as usize;
                let take = remaining.min(room);
                last.length += take as u32;
                last.physical_end = physical + take;
                in_page += take;
                remaining -= take;
            } else {
                let take = remaining.min(max_segment as usize);
                segments.push(Segment {
                    page,
                    offset: in_page as u32,
                    length: take as u32,
                    physical_end: physical + take,
                });
                in_page += take;
                remaining -= take;
            }
        }
    }

    let Ok(count) = u32::try_from(segments.len()) else {
        return -EINVAL;
    };
    let status = unsafe { sg_alloc_table(table, count, gfp_mask) };
    if status != 0 {
        return status;
    }
    unsafe {
        let mut sg = (*table).sgl;
        for segment in &segments {
            (*sg).page_link = segment.page as usize;
            (*sg).offset = segment.offset;
            (*sg).length = segment.length;
            sg = sg.add(1);
        }
        sg_mark_end((*table).sgl.add(count as usize - 1));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        i915_gem_pages_upstream::sg_page,
        linux::shmem::{alloc_pages, free_pages, pfn_to_page, try_page_to_phys},
        linux_config::GFP_KERNEL,
    };

    fn empty_table() -> SgTable {
        SgTable {
            sgl: ptr::null_mut(),
            nents: 0,
            orig_nents: 0,
        }
    }

    #[test]
    fn contiguous_pages_merge_and_split_at_max_segment() {
        unsafe {
            let head = alloc_pages(GFP_KERNEL, 2);
            assert!(!head.is_null());
            let base = try_page_to_phys(head).expect("compound head is registered");
            let mut pages = [ptr::null_mut::<Page>(); 4];
            for (index, slot) in pages.iter_mut().enumerate() {
                *slot = pfn_to_page(((base >> PAGE_SHIFT) + index) as c_ulong);
                assert!(!slot.is_null());
            }

            let mut table = empty_table();
            let whole = 4 * PAGE_SIZE as c_ulong;
            assert_eq!(
                sg_alloc_table_from_pages_segment(
                    &mut table,
                    pages.as_mut_ptr(),
                    4,
                    0,
                    whole,
                    u32::MAX,
                    GFP_KERNEL,
                ),
                0
            );
            assert_eq!(table.nents, 1);
            assert_eq!((*table.sgl).length as usize, 4 * PAGE_SIZE);
            assert_eq!(sg_page(table.sgl), pages[0]);
            sg_free_table(&mut table);
            assert!(table.sgl.is_null());

            assert_eq!(
                sg_alloc_table_from_pages_segment(
                    &mut table,
                    pages.as_mut_ptr(),
                    4,
                    0,
                    whole,
                    2 * PAGE_SIZE as u32,
                    GFP_KERNEL,
                ),
                0
            );
            assert_eq!(table.nents, 2);
            assert_eq!((*table.sgl).length as usize, 2 * PAGE_SIZE);
            assert_eq!((*sg_next(table.sgl)).length as usize, 2 * PAGE_SIZE);
            sg_free_table(&mut table);

            // A window starting mid-page keeps the byte offset on the first segment.
            assert_eq!(
                sg_alloc_table_from_pages_segment(
                    &mut table,
                    pages.as_mut_ptr(),
                    4,
                    1000,
                    2 * PAGE_SIZE as c_ulong,
                    u32::MAX,
                    GFP_KERNEL,
                ),
                0
            );
            assert_eq!(table.nents, 1);
            assert_eq!((*table.sgl).offset, 1000);
            assert_eq!((*table.sgl).length as usize, 2 * PAGE_SIZE);
            sg_free_table(&mut table);

            // Ranges past the supplied pages are refused.
            assert_eq!(
                sg_alloc_table_from_pages_segment(
                    &mut table,
                    pages.as_mut_ptr(),
                    4,
                    0,
                    5 * PAGE_SIZE as c_ulong,
                    u32::MAX,
                    GFP_KERNEL,
                ),
                -EINVAL
            );

            free_pages(head, 2);
        }
    }

    #[test]
    fn sg_alloc_table_rejects_zero_and_marks_end() {
        unsafe {
            let mut table = empty_table();
            assert_eq!(sg_alloc_table(&mut table, 0, GFP_KERNEL), -EINVAL);
            assert_eq!(sg_alloc_table(&mut table, 3, GFP_KERNEL), 0);
            assert_eq!(table.nents, 3);
            assert_eq!(table.orig_nents, 3);
            let last = table.sgl.add(2);
            assert!(sg_next(last).is_null());
            sg_free_table(&mut table);
        }
    }
}
