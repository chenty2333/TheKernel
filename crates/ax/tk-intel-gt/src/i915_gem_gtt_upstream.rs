// SPDX-License-Identifier: MIT
// Copyright © 2014-2016 Intel Corporation.
//! Linux 7.2.3 i915_gem_gtt.c DMA page prepare/finish, in source order.
#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]
use core::ffi::{c_int, c_ulong, c_void};

use crate::{
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_gem_shrinker_upstream::i915_gem_shrink,
    intel_context_upstream::{DrmMmNode, SgTable},
    intel_gtt_api_upstream::{I915_GTT_MIN_ALIGNMENT, I915_GTT_PAGE_SIZE, I915AddressSpace},
    i915_gem_evict_upstream::{i915_gem_evict_for_node, i915_gem_evict_something},
    i915_gem_ww_upstream::I915GemWwCtx,
    i915_vma_upstream::{PIN_HIGH, PIN_MAPPABLE, PIN_NOEVICT, PIN_NOSEARCH},
    linux::{
        dma::{dma_map_sg_attrs, dma_unmap_sg_attrs},
        gem::drm_device_device,
        gem_memory::drm_mm_node_allocated,
        i915::to_i915,
    },
    linux_config::ENOSPC,
};

// Linux `DRM_MM_INSERT_*` (include/drm/drm_mm.h).
const DRM_MM_INSERT_BEST: u32 = 0;
const DRM_MM_INSERT_LOW: u32 = 1;
const DRM_MM_INSERT_HIGH: u32 = 2;
const DRM_MM_INSERT_EVICT: u32 = 3;
const DRM_MM_INSERT_ONCE: u32 = 1 << 31;
const DRM_MM_INSERT_HIGHEST: u32 = DRM_MM_INSERT_HIGH | DRM_MM_INSERT_ONCE;

unsafe extern "C" {
    fn drm_mm_reserve_node(mm: *mut c_void, node: *mut DrmMmNode) -> c_int;
    fn drm_mm_insert_node_in_range(
        mm: *mut c_void,
        node: *mut DrmMmNode,
        size: u64,
        alignment: u64,
        color: c_ulong,
        start: u64,
        end: u64,
        mode: u32,
    ) -> c_int;
    // LinuxKPI random source (L2 owner); range is exclusive, as in `get_random_u32_below()`.
    fn get_random_u32_below(range: u32) -> u32;
}

// upstream: i915_gem_gtt.c range_overflows() (include/linux/overflow helper)
fn range_overflows(start: u64, size: u64, end: u64) -> bool {
    start > u64::MAX - size || start + size > end
}

// Linux round_up()/round_down() mask arithmetic. `align == 0` is reachable from
// i915_gem_gtt_insert() and yields 0, exactly as the C macros do.
fn round_up(x: u64, align: u64) -> u64 {
    x.wrapping_add(align).wrapping_sub(1) & !align.wrapping_sub(1)
}

fn round_down(x: u64, align: u64) -> u64 {
    x & !align.wrapping_sub(1)
}

// get_random_u64() from the source, composed from the 32-bit LinuxKPI source.
fn get_random_u64() -> u64 {
    let hi = unsafe { get_random_u32_below(u32::MAX) } as u64;
    let lo = unsafe { get_random_u32_below(u32::MAX) } as u64;
    (hi << 32) | lo
}

// upstream: i915_gem_gtt.c i915_gem_gtt_reserve()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_gtt_reserve(
    vm: *mut I915AddressSpace,
    ww: *mut I915GemWwCtx,
    node: *mut DrmMmNode,
    size: u64,
    offset: u64,
    color: c_ulong,
    flags: u64,
) -> c_int {
    GEM_BUG_ON!(size == 0);
    GEM_BUG_ON!(size % I915_GTT_PAGE_SIZE != 0);
    GEM_BUG_ON!(offset % I915_GTT_MIN_ALIGNMENT != 0);
    GEM_BUG_ON!(range_overflows(offset, size, unsafe { (*vm).total }));
    GEM_BUG_ON!(unsafe { drm_mm_node_allocated(node) });

    unsafe {
        (*node).size = size;
        (*node).start = offset;
        (*node).color = color;
    }

    let mut err = unsafe { drm_mm_reserve_node(core::ptr::addr_of_mut!((*vm).mm).cast(), node) };
    if err != -ENOSPC {
        return err;
    }

    if flags & PIN_NOEVICT != 0 {
        return -ENOSPC;
    }

    err = unsafe { i915_gem_evict_for_node(vm, ww, node, flags as u32) };
    if err == 0 {
        err = unsafe { drm_mm_reserve_node(core::ptr::addr_of_mut!((*vm).mm).cast(), node) };
    }

    err
}

// upstream: i915_gem_gtt.c random_offset()
unsafe fn random_offset(start: u64, end: u64, len: u64, align: u64) -> u64 {
    GEM_BUG_ON!(range_overflows(start, len, end));
    GEM_BUG_ON!(round_up(start, align) > round_down(end - len, align));

    let range = round_down(end - len, align) - round_up(start, align);
    let mut start = start;
    if range != 0 {
        let addr = get_random_u64() % range;
        start += addr;
    }

    round_up(start, align)
}

// upstream: i915_gem_gtt.c i915_gem_gtt_insert()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_gtt_insert(
    vm: *mut I915AddressSpace,
    ww: *mut I915GemWwCtx,
    node: *mut DrmMmNode,
    size: u64,
    mut alignment: u64,
    color: c_ulong,
    start: u64,
    end: u64,
    flags: u64,
) -> c_int {
    GEM_BUG_ON!(size == 0);
    GEM_BUG_ON!(size % I915_GTT_PAGE_SIZE != 0);
    GEM_BUG_ON!(alignment != 0 && !alignment.is_power_of_two());
    GEM_BUG_ON!(alignment != 0 && alignment % I915_GTT_MIN_ALIGNMENT != 0);
    GEM_BUG_ON!(start >= end);
    GEM_BUG_ON!(start > 0 && start % I915_GTT_PAGE_SIZE != 0);
    GEM_BUG_ON!(end < u64::MAX && end % I915_GTT_PAGE_SIZE != 0);
    GEM_BUG_ON!(unsafe { drm_mm_node_allocated(node) });

    if range_overflows(start, size, end) {
        return -ENOSPC;
    }

    if round_up(start, alignment) > round_down(end - size, alignment) {
        return -ENOSPC;
    }

    let mut mode = DRM_MM_INSERT_BEST;
    if flags & PIN_HIGH != 0 {
        mode = DRM_MM_INSERT_HIGHEST;
    }
    if flags & PIN_MAPPABLE != 0 {
        mode = DRM_MM_INSERT_LOW;
    }

    // Allocations are always 4 KiB chunks, so the optimal drm_mm path uses zero alignment.
    if alignment <= I915_GTT_MIN_ALIGNMENT {
        alignment = 0;
    }

    let mm = unsafe { core::ptr::addr_of_mut!((*vm).mm).cast::<c_void>() };
    let mut err = unsafe {
        drm_mm_insert_node_in_range(mm, node, size, alignment, color, start, end, mode)
    };
    if err != -ENOSPC {
        return err;
    }

    if mode & DRM_MM_INSERT_ONCE != 0 {
        err = unsafe {
            drm_mm_insert_node_in_range(
                mm,
                node,
                size,
                alignment,
                color,
                start,
                end,
                DRM_MM_INSERT_BEST,
            )
        };
        if err != -ENOSPC {
            return err;
        }
    }

    if flags & PIN_NOEVICT != 0 {
        return -ENOSPC;
    }

    // No free space: try one random slot before a full eviction search.
    let offset = unsafe {
        random_offset(
            start,
            end,
            size,
            if alignment != 0 { alignment } else { I915_GTT_MIN_ALIGNMENT },
        )
    };
    err = unsafe { i915_gem_gtt_reserve(vm, ww, node, size, offset, color, flags) };
    if err != -ENOSPC {
        return err;
    }

    if flags & PIN_NOSEARCH != 0 {
        return -ENOSPC;
    }

    // Randomly selected placement is pinned, do a search.
    err = unsafe {
        i915_gem_evict_something(vm, ww, size, alignment, color, start, end, flags as u32)
    };
    if err != 0 {
        return err;
    }

    unsafe {
        drm_mm_insert_node_in_range(mm, node, size, alignment, color, start, end, DRM_MM_INSERT_EVICT)
    }
}

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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_gtt_finish_pages(obj: *mut DrmI915GemObject, pages: *mut SgTable) {
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
