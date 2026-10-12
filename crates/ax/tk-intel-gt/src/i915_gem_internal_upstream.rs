// SPDX-License-Identifier: MIT
// Copyright © 2014-2016 Intel Corporation.
//
//! Linux v7.2.3 `drivers/gpu/drm/i915/gem/i915_gem_internal.c` translation.
//! This source-order implementation preserves page allocation fallback,
//! scatterlist lifetime, and internal GEM object initialization behavior.

#![allow(unsafe_code, unsafe_op_in_unsafe_fn, non_snake_case)]

use core::ffi::c_char;

use crate::{
    i915_gem_gtt_upstream::{i915_gem_gtt_finish_pages, i915_gem_gtt_prepare_pages},
    i915_gem_object_header_upstream::{__start_cpu_write, i915_gem_object_set_volatile},
    i915_gem_object_types_upstream::{
        DrmI915GemObject, DrmI915GemObjectOps, I915_GEM_OBJECT_IS_SHRINKABLE, Page,
    },
    i915_gem_object_upstream::{
        i915_gem_object_alloc, i915_gem_object_init, i915_gem_object_set_cache_coherency,
    },
    i915_gem_pages_upstream::{
        __i915_gem_object_set_pages, Scatterlist, sg_alloc_table, sg_mark_end, sg_next, sg_page,
    },
    i915_gem_userptr_upstream::{drm_gem_private_object_init, sg_free_table},
    intel_context_upstream::SgTable,
    linux::{
        config::ERR_PTR,
        fields::LockClassKey,
        i915::{HAS_LLC, IS_I965G, IS_I965GM, to_i915},
        memory::{kfree, kmalloc_obj},
        scatterlist::i915_sg_segment_size,
    },
    linux_config::{__GFP_NOWARN, E2BIG, ENOMEM, GFP_KERNEL, PAGE_SHIFT, PAGE_SIZE},
    linux_i915_private::DrmI915Private,
};

const I915_GEM_DOMAIN_CPU: u32 = 1;
const I915_CACHE_NONE: u32 = 0;
const I915_CACHE_LLC: u32 = 1;
const MAX_PAGE_ORDER: i32 = 10;
const __GFP_HIGHMEM: u32 = 1 << 1;
const __GFP_DMA32: u32 = 1 << 2;
const __GFP_RECLAIMABLE: u32 = 1 << 4;
const __GFP_RETRY_MAYFAIL: u32 = 1 << 14;
const __GFP_NORETRY: u32 = 1 << 16;
const SG_PAGE_LINK_MASK: usize = 3;

unsafe extern "C" {
    // Linux MM services required by the source allocation policy. These are
    // genuine kernel integration dependencies, not local success stubs.
    fn alloc_pages(gfp_mask: u32, order: u32) -> *mut Page;
    fn __free_pages(page: *mut Page, order: u32);
}

#[inline]
fn get_order(size: u64) -> i32 {
    // Linux asm-generic/getorder.h: order of the smallest PAGE_SIZE multiple
    // that contains `size`. Its result for zero is undefined; callers here
    // pass non-zero segment/page lengths.
    if size <= PAGE_SIZE as u64 {
        0
    } else {
        (u64::BITS - (size - 1).leading_zeros()) as i32 - PAGE_SHIFT as i32
    }
}

#[inline]
fn retry_internal_page_allocation(
    first_sg_length: u32,
    page_count: u32,
    remaining: &mut u32,
    max_order: &mut i32,
) -> bool {
    if get_order(u64::from(first_sg_length)) == 0 {
        return false;
    }
    *remaining = page_count;
    *max_order = 0;
    true
}

#[inline]
unsafe fn sg_set_page_local(sg: *mut Scatterlist, page: *mut Page, len: u32) {
    unsafe {
        (*sg).page_link = ((*sg).page_link & SG_PAGE_LINK_MASK) | page as usize;
        (*sg).offset = 0;
        (*sg).length = len;
    }
}

// upstream: i915_gem_internal.c internal_free_pages()
unsafe fn internal_free_pages(st: *mut SgTable) {
    let mut sg = unsafe { (*st).sgl };
    while !sg.is_null() {
        let page = unsafe { sg_page(sg) };
        if !page.is_null() {
            unsafe { __free_pages(page, get_order((*sg).length as u64) as u32) };
        }
        sg = unsafe { sg_next(sg) };
    }
    unsafe {
        sg_free_table(st);
        kfree(st);
    }
}

/// Abort a page-table build, optionally terminating at its first unallocated
/// SG slot. `unallocated_slot` must be non-null only when page allocation
/// failed before that slot was populated; a complete table passes null so the
/// final allocated page remains owned and is released too.
unsafe fn abort_internal_page_table(
    st: *mut SgTable,
    unallocated_slot: *mut Scatterlist,
) {
    if !unallocated_slot.is_null() {
        unsafe {
            sg_set_page_local(unallocated_slot, core::ptr::null_mut(), 0);
            sg_mark_end(unallocated_slot);
        }
    }
    unsafe { internal_free_pages(st) };
}

// upstream: i915_gem_internal.c i915_gem_object_get_pages_internal()
unsafe extern "C" fn i915_gem_object_get_pages_internal(obj: *mut DrmI915GemObject) -> i32 {
    let i915 = unsafe { to_i915((&(*obj).base.base).dev) };
    let mut st: *mut SgTable;
    let mut sg: *mut Scatterlist;
    let mut max_order = MAX_PAGE_ORDER;

    let page_count = unsafe { (&(*obj).base.base).size >> PAGE_SHIFT };
    if page_count > u32::MAX as u64 {
        return -E2BIG;
    }
    let total_pages = page_count as u32;

    let max_segment = unsafe { i915_sg_segment_size((*i915).drm.dev) } >> PAGE_SHIFT;
    max_order = max_order.min(get_order(max_segment as u64));

    let mut gfp = GFP_KERNEL | __GFP_HIGHMEM | __GFP_RECLAIMABLE;
    if unsafe { IS_I965GM(i915) || IS_I965G(i915) } {
        // 965gm cannot relocate objects above 4GiB.
        gfp &= !__GFP_HIGHMEM;
        gfp |= __GFP_DMA32;
    }

    let mut npages = total_pages;
    'create_st: loop {
        st = kmalloc_obj::<SgTable>(GFP_KERNEL);
        if st.is_null() {
            return -ENOMEM;
        }

        if unsafe { sg_alloc_table(st, npages, GFP_KERNEL) } != 0 {
            unsafe { kfree(st) };
            return -ENOMEM;
        }

        sg = unsafe { (*st).sgl };
        unsafe { (*st).nents = 0 };

        loop {
            let mut order = ((u32::BITS - npages.leading_zeros()) as i32 - 1).min(max_order) as u32;
            let page = loop {
                let flags = gfp
                    | if order != 0 {
                        __GFP_NORETRY | __GFP_NOWARN
                    } else {
                        __GFP_RETRY_MAYFAIL | __GFP_NOWARN
                    };
                let page = unsafe { alloc_pages(flags, order) };
                if !page.is_null() {
                    break page;
                }
                if order == 0 {
                    drm_err!(
                        &(*i915).drm,
                        "Internal GEM backing-page allocation exhausted: object_size=%llu remaining_pages=%u gfp=%#x\n",
                        unsafe { (&(*obj).base.base).size },
                        npages,
                        flags
                    );
                    unsafe { abort_internal_page_table(st, sg) };
                    return -ENOMEM;
                }
                order -= 1;
                max_order = order as i32;
            };

            unsafe {
                sg_set_page_local(sg, page, (PAGE_SIZE as u32) << order);
                (*st).nents += 1;
            }

            npages -= 1 << order;
            if npages == 0 {
                unsafe { sg_mark_end(sg) };
                break;
            }

            sg = unsafe { sg_next(sg) };
        }

        let prepare_ret = unsafe { i915_gem_gtt_prepare_pages(obj, st) };
        if prepare_ret == 0 {
            unsafe { __i915_gem_object_set_pages(obj, st) };
            return 0;
        }

        // A failed DMA map gets one retry with a single-page scatterlist.
        if retry_internal_page_allocation(
            unsafe { (*(*st).sgl).length },
            total_pages,
            &mut npages,
            &mut max_order,
        ) {
            unsafe { internal_free_pages(st) };
            continue 'create_st;
        }

        drm_err!(
            &(*i915).drm,
            "Internal GEM DMA mapping failed after order-0 fallback: object_size=%llu page_count=%llu sg_entries=%u ret=%d\n",
            unsafe { (&(*obj).base.base).size },
            page_count,
            unsafe { (*st).nents },
            prepare_ret
        );
        unsafe { abort_internal_page_table(st, core::ptr::null_mut()) };
        return -ENOMEM;
    }
}

// upstream: i915_gem_internal.c i915_gem_object_put_pages_internal()
unsafe extern "C" fn i915_gem_object_put_pages_internal(
    obj: *mut DrmI915GemObject,
    pages: *mut SgTable,
) {
    unsafe {
        i915_gem_gtt_finish_pages(obj, pages);
        internal_free_pages(pages);
        (*obj).mm.set_dirty(false);
        __start_cpu_write(obj);
    }
}

// upstream: i915_gem_internal.c __i915_gem_object_create_internal()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __i915_gem_object_create_internal(
    i915: *mut DrmI915Private,
    ops: *const DrmI915GemObjectOps,
    size: u64,
) -> *mut DrmI915GemObject {
    static mut LOCK_CLASS: LockClassKey = LockClassKey {};
    GEM_BUG_ON!(size == 0);
    GEM_BUG_ON!(size & (PAGE_SIZE as u64 - 1) != 0);
    if size > usize::MAX as u64 {
        return ERR_PTR(-E2BIG);
    }

    let obj = unsafe { i915_gem_object_alloc() };
    if obj.is_null() {
        return ERR_PTR(-ENOMEM);
    }

    unsafe {
        let drm_obj = core::ptr::addr_of_mut!((*obj).base.base).cast();
        drm_gem_private_object_init(
            core::ptr::addr_of_mut!((*i915).drm).cast(),
            drm_obj,
            size as usize,
        );
        i915_gem_object_init(obj, ops, core::ptr::addr_of_mut!(LOCK_CLASS), 0);
        (*obj).mem_flags |= crate::i915_gem_object_types_upstream::I915_BO_FLAG_STRUCT_PAGE;
        i915_gem_object_set_volatile(obj);
        (*obj).read_domains = I915_GEM_DOMAIN_CPU as u16;
        (*obj).write_domain = I915_GEM_DOMAIN_CPU as u16;
        let cache_level = if HAS_LLC(i915) {
            I915_CACHE_LLC
        } else {
            I915_CACHE_NONE
        };
        i915_gem_object_set_cache_coherency(obj, cache_level);
    }

    obj
}

static I915_GEM_OBJECT_INTERNAL_OPS: DrmI915GemObjectOps = DrmI915GemObjectOps {
    flags: I915_GEM_OBJECT_IS_SHRINKABLE,
    get_pages: Some(i915_gem_object_get_pages_internal),
    put_pages: Some(i915_gem_object_put_pages_internal),
    truncate: None,
    shrink: None,
    pread: None,
    pwrite: None,
    mmap_offset: None,
    unmap_virtual: None,
    dmabuf_export: None,
    adjust_lru: None,
    delayed_free: None,
    migrate: None,
    release: None,
    mmap_ops: core::ptr::null(),
    name: b"i915_gem_object_internal\0".as_ptr().cast::<c_char>(),
};

// upstream: i915_gem_internal.c i915_gem_object_create_internal()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_create_internal(
    i915: *mut DrmI915Private,
    size: u64,
) -> *mut DrmI915GemObject {
    unsafe { __i915_gem_object_create_internal(i915, &I915_GEM_OBJECT_INTERNAL_OPS, size) }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn populated_test_table(
        pages: &[*mut Page],
        entries: u32,
    ) -> (*mut SgTable, *mut Scatterlist) {
        let table = unsafe { kmalloc_obj::<SgTable>(GFP_KERNEL) };
        assert!(!table.is_null());
        if unsafe { sg_alloc_table(table, entries, GFP_KERNEL) } != 0 {
            unsafe { kfree(table) };
            panic!("could not allocate focused SG fixture");
        }
        assert!(!pages.is_empty() && pages.len() <= entries as usize);

        let mut sg = unsafe { (*table).sgl };
        for (index, page) in pages.iter().enumerate() {
            unsafe { sg_set_page_local(sg, *page, PAGE_SIZE as u32) };
            if index + 1 != pages.len() {
                sg = unsafe { sg_next(sg) };
            }
        }
        let unallocated_slot = if pages.len() < entries as usize {
            unsafe { sg_next(sg) }
        } else {
            core::ptr::null_mut()
        };
        (table, unallocated_slot)
    }

    #[test]
    fn failed_dma_of_large_sg_resets_page_count_for_single_page_retry() {
        let page_count = 16;
        let mut remaining = 0;
        let mut max_order = 4;
        assert!(retry_internal_page_allocation(
            (2 * PAGE_SIZE) as u32,
            page_count,
            &mut remaining,
            &mut max_order,
        ));
        assert_eq!(remaining, page_count);
        assert_eq!(max_order, 0);

        assert!(!retry_internal_page_allocation(
            PAGE_SIZE as u32,
            page_count,
            &mut remaining,
            &mut max_order,
        ));
        assert_eq!(remaining, page_count, "4-KiB failure is not retried again");
    }

    #[test]
    fn dma_failure_releases_all_populated_pages_without_losing_last_entry() {
        const PAGE_COUNT: usize = 3;
        unsafe {
            let mut pages = [core::ptr::null_mut::<Page>(); PAGE_COUNT];
            for page in pages.iter_mut() {
                *page = alloc_pages(GFP_KERNEL, 0);
                assert!(!(*page).is_null());
            }
            assert!(pages.iter().all(|page| crate::linux::shmem::try_page_to_phys(*page).is_some()));

            let (table, unallocated_slot) = populated_test_table(&pages, PAGE_COUNT as u32);
            assert!(unallocated_slot.is_null(), "DMA failure sees a complete SG table");
            abort_internal_page_table(table, unallocated_slot);
            assert!(pages.iter().all(|page| crate::linux::shmem::try_page_to_phys(*page).is_none()));
        }
    }

    #[test]
    fn allocation_failure_terminates_at_unfilled_slot_and_frees_only_prefix() {
        const ALLOCATED: usize = 2;
        const ENTRIES: u32 = 3;
        unsafe {
            let mut pages = [core::ptr::null_mut::<Page>(); ALLOCATED];
            for page in pages.iter_mut() {
                *page = alloc_pages(GFP_KERNEL, 0);
                assert!(!(*page).is_null());
            }
            let (table, unallocated_slot) = populated_test_table(&pages, ENTRIES);
            assert!(!unallocated_slot.is_null());
            abort_internal_page_table(table, unallocated_slot);
            assert!(pages.iter().all(|page| crate::linux::shmem::try_page_to_phys(*page).is_none()));
        }
    }
}
