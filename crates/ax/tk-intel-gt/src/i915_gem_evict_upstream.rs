// Copyright © 2008-2010 Intel Corporation.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice (including the next
// paragraph) shall be included in all copies or substantial portions of the
// Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
// THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
// IN THE SOFTWARE.
//
// Source-order Rust translation of Linux v7.2.3
// drivers/gpu/drm/i915/i915_gem_evict.c.
#![allow(
    unsafe_code,
    unsafe_op_in_unsafe_fn,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    unexpected_cfgs
)]

use core::{
    ffi::{c_int, c_long, c_ulong},
    mem::{offset_of, size_of},
    ptr,
    sync::atomic::{AtomicI32, Ordering},
};

use crate::{
    i915_gem_object_api_upstream::{
        i915_gem_object_get_rcu, i915_gem_object_put, i915_gem_object_trylock,
        i915_gem_object_unlock,
    },
    i915_gem_object_types_upstream::{DrmI915GemObject, intel_bo_to_drm_bo},
    i915_gem_ww_upstream::I915GemWwCtx,
    i915_vma_api_upstream::{
        __i915_vma_pin, __i915_vma_unbind, __i915_vma_unpin, i915_vma_is_active,
        i915_vma_is_pinned, i915_vma_is_scanout,
    },
    i915_vma_types_upstream::{I915_VMA_PIN_MASK, I915Vma},
    intel_context_upstream::{DrmGemObjectBaseLayout, DrmMmNode},
    intel_engine_cs_upstream::ListHead,
    intel_gt_requests_upstream::intel_gt_retire_requests_timeout,
    intel_gt_types_upstream::IntelGt,
    intel_gt_upstream::intel_gt_wait_for_idle,
    intel_gtt_api_upstream::{
        I915_GTT_PAGE_SIZE, I915AddressSpace, I915Ggtt, i915_is_ggtt, i915_vm_has_cache_coloring,
        i915_vm_to_ggtt,
    },
    linux::{
        gem::DmaResv,
        gem_memory::{DrmMm, drm_mm_node_allocated},
        memory::kref_read,
    },
    linux_config::{
        EAGAIN, EBUSY, EINTR, ENOSPC, MAX_SCHEDULE_TIMEOUT, PIN_HIGH, PIN_MAPPABLE, PIN_NONBLOCK,
    },
    linux_list::*,
    linux_wait::cond_resched,
};

const DRM_MM_INSERT_BEST: c_int = 0;
const DRM_MM_INSERT_LOW: c_int = 1;
const DRM_MM_INSERT_HIGH: c_int = 2;
const I915_COLOR_UNEVICTABLE: c_ulong = c_ulong::MAX;

/// DRM-MM scan state is documented as opaque in Linux, but its source layout
/// is fixed by `drm_mm.h` and its only operations below are the real DRM-MM
/// scan APIs. It is a stack temporary, exactly as in the C owner.
#[repr(C)]
struct DrmMmScan {
    mm: *mut DrmMm,
    size: u64,
    alignment: u64,
    remainder_mask: u64,
    range_start: u64,
    range_end: u64,
    hit_start: u64,
    hit_end: u64,
    color: c_ulong,
    mode: c_int,
    _padding: u32,
}
const _: [(); 80] = [(); size_of::<DrmMmScan>()];
const _: [(); 0] = [(); offset_of!(DrmMmScan, mm)];
const _: [(); 8] = [(); offset_of!(DrmMmScan, size)];
const _: [(); 64] = [(); offset_of!(DrmMmScan, color)];
const _: [(); 72] = [(); offset_of!(DrmMmScan, mode)];

// The selftest declaration is `I915_SELFTEST_DECLARE(...)` in C and expands
// away for this target's CONFIG_DRM_I915_SELFTEST=n configuration. It is not a
// function definition (ctags emits it as a false-positive pseudo-function).
#[cfg(CONFIG_DRM_I915_SELFTEST)]
#[repr(C)]
struct IgtEvictCtl {
    fail_if_busy: bool,
}
#[cfg(CONFIG_DRM_I915_SELFTEST)]
static mut igt_evict_ctl: IgtEvictCtl = IgtEvictCtl {
    fail_if_busy: false,
};

unsafe extern "C" {
    // These calls are Linux DRM-MM services, not local eviction substitutes.
    fn drm_mm_scan_init_with_range(
        scan: *mut DrmMmScan,
        mm: *mut DrmMm,
        size: u64,
        alignment: u64,
        color: c_ulong,
        start: u64,
        end: u64,
        mode: c_int,
    );
    fn drm_mm_scan_add_block(scan: *mut DrmMmScan, node: *mut DrmMmNode) -> bool;
    fn drm_mm_scan_remove_block(scan: *mut DrmMmScan, node: *mut DrmMmNode) -> bool;
    fn drm_mm_scan_color_evict(scan: *mut DrmMmScan) -> *mut DrmMmNode;
    fn __drm_mm_interval_first(mm: *const DrmMm, start: u64, last: u64) -> *mut DrmMmNode;

    fn trace_i915_gem_evict(vm: *mut I915AddressSpace, size: u64, alignment: u64, flags: u32);
    fn trace_i915_gem_evict_node(vm: *mut I915AddressSpace, target: *mut DrmMmNode, flags: u32);
    fn trace_i915_gem_evict_vm(vm: *mut I915AddressSpace);
}

#[inline]
fn err_ptr<T>(err: c_int) -> *mut T {
    err as isize as *mut T
}

#[inline]
unsafe fn object_refcount(obj: *mut DrmI915GemObject) -> u32 {
    let base = unsafe { intel_bo_to_drm_bo(obj) };
    unsafe { kref_read(&(*base).refcount) }
}

#[inline]
unsafe fn object_locked_by_ww(obj: *mut DrmI915GemObject, ww: *mut I915GemWwCtx) -> bool {
    let base = obj.cast::<DrmGemObjectBaseLayout>();
    let resv = unsafe { (*base).resv.cast::<DmaResv>() };
    // dma_resv_locking_ctx() is a C inline READ_ONCE of ww_mutex.ctx.
    let lock_ctx = unsafe { ptr::read_volatile(ptr::addr_of!((*resv).lock.ctx)) };
    lock_ctx == unsafe { ptr::addr_of_mut!((*ww).ctx) }
}

#[inline]
unsafe fn retire_gt_requests(gt: *mut IntelGt) {
    // This is precisely intel_gt_requests.h's inline wrapper, which ignores
    // the zero-timeout result from the out-of-line retirement service.
    let _ = unsafe { intel_gt_retire_requests_timeout(gt, 0, ptr::null_mut()) };
}

// upstream: i915_gem_evict.c dying_vma()
unsafe fn dying_vma(vma: *mut I915Vma) -> bool {
    unsafe { object_refcount((*vma).obj) == 0 }
}

// upstream: i915_gem_evict.c ggtt_flush()
unsafe fn ggtt_flush(vm: *mut I915AddressSpace) -> c_int {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    let head = ptr::addr_of_mut!((*ggtt).gt_list);
    let mut gt: *mut IntelGt = ptr::null_mut();
    let mut ret = 0;
    list_for_each_entry!(gt, head, ggtt_link, {
        ret = unsafe { intel_gt_wait_for_idle(gt, MAX_SCHEDULE_TIMEOUT as c_long) };
        if ret != 0 {
            break;
        }
    });
    ret
}

// upstream: i915_gem_evict.c grab_vma()
unsafe fn grab_vma(vma: *mut I915Vma, ww: *mut I915GemWwCtx) -> bool {
    let obj = unsafe { (*vma).obj };
    if !unsafe { i915_gem_object_get_rcu(obj) }.is_null() {
        if !unsafe { i915_gem_object_trylock(obj, ww) } {
            unsafe { i915_gem_object_put(obj) };
            return false;
        }
    } else {
        // Dead objects have no refcount owner left and no locks to acquire.
        unsafe {
            AtomicI32::from_ptr(ptr::addr_of_mut!((*vma).flags.counter))
                .fetch_and(!I915_VMA_PIN_MASK, Ordering::SeqCst);
        }
    }
    true
}

// upstream: i915_gem_evict.c ungrab_vma()
unsafe fn ungrab_vma(vma: *mut I915Vma) {
    if unsafe { dying_vma(vma) } {
        return;
    }
    unsafe {
        i915_gem_object_unlock((*vma).obj);
        i915_gem_object_put((*vma).obj);
    }
}

// upstream: i915_gem_evict.c mark_free()
unsafe fn mark_free(
    scan: *mut DrmMmScan,
    ww: *mut I915GemWwCtx,
    vma: *mut I915Vma,
    _flags: u32,
    unwind: *mut ListHead,
) -> bool {
    if unsafe { i915_vma_is_pinned(vma) } {
        return false;
    }
    if !unsafe { grab_vma(vma, ww) } {
        return false;
    }
    unsafe { list_add(ptr::addr_of_mut!((*vma).evict_link), unwind) };
    unsafe { drm_mm_scan_add_block(scan, ptr::addr_of_mut!((*vma).node)) }
}

// upstream: i915_gem_evict.c defer_evict()
unsafe fn defer_evict(vma: *mut I915Vma) -> bool {
    unsafe { i915_vma_is_active(vma) || i915_vma_is_scanout(vma) }
}

// upstream: i915_gem_evict.c i915_gem_evict_something()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_evict_something(
    vm: *mut I915AddressSpace,
    ww: *mut I915GemWwCtx,
    min_size: u64,
    alignment: u64,
    color: c_ulong,
    start: u64,
    end: u64,
    mut flags: u32,
) -> c_int {
    lockdep_assert_held!(unsafe { &(*vm).mutex });
    unsafe { trace_i915_gem_evict(vm, min_size, alignment, flags) };

    let mut mode = DRM_MM_INSERT_BEST;
    if flags & PIN_HIGH as u32 != 0 {
        mode = DRM_MM_INSERT_HIGH;
    }
    if flags & PIN_MAPPABLE as u32 != 0 {
        mode = DRM_MM_INSERT_LOW;
    }
    let mut scan_storage = core::mem::MaybeUninit::<DrmMmScan>::uninit();
    let scan = scan_storage.as_mut_ptr();
    unsafe {
        drm_mm_scan_init_with_range(
            scan,
            ptr::addr_of_mut!((*vm).mm),
            min_size,
            alignment,
            color,
            start,
            end,
            mode,
        );
    }

    if i915_is_ggtt(vm) {
        let ggtt = unsafe { i915_vm_to_ggtt(vm) };
        let mut gt: *mut IntelGt = ptr::null_mut();
        list_for_each_entry!(gt, ptr::addr_of_mut!((*ggtt).gt_list), ggtt_link, {
            unsafe { retire_gt_requests(gt) };
        });
    } else {
        unsafe { retire_gt_requests((*vm).gt) };
    }

    let retry_sentinel = err_ptr::<I915Vma>(-EAGAIN);
    loop {
        let mut active: *mut I915Vma = ptr::null_mut();
        let mut eviction_list = ListHead {
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
        };
        INIT_LIST_HEAD!(&mut eviction_list);

        let mut vma: *mut I915Vma = ptr::null_mut();
        let mut next: *mut I915Vma = ptr::null_mut();
        let mut found = false;
        list_for_each_entry_safe!(vma, next, ptr::addr_of_mut!((*vm).bound_list), vm_link, {
            if vma == active {
                if flags & PIN_NONBLOCK as u32 != 0 {
                    break;
                }
                active = retry_sentinel;
            }

            if active != retry_sentinel && unsafe { defer_evict(vma) } {
                if active.is_null() {
                    active = vma;
                }
                unsafe {
                    list_move_tail(
                        ptr::addr_of_mut!((*vma).vm_link),
                        ptr::addr_of_mut!((*vm).bound_list),
                    );
                }
                continue;
            }

            if unsafe { mark_free(scan, ww, vma, flags, &mut eviction_list) } {
                found = true;
                break;
            }
        });

        if !found {
            let mut vma: *mut I915Vma = ptr::null_mut();
            let mut next: *mut I915Vma = ptr::null_mut();
            list_for_each_entry_safe!(vma, next, &mut eviction_list, evict_link, {
                let removed =
                    unsafe { drm_mm_scan_remove_block(scan, ptr::addr_of_mut!((*vma).node)) };
                BUG_ON!(removed);
                unsafe { ungrab_vma(vma) };
            });

            if !i915_is_ggtt(vm) || flags & PIN_NONBLOCK as u32 != 0 {
                return -ENOSPC;
            }

            #[cfg(CONFIG_DRM_I915_SELFTEST)]
            if unsafe { igt_evict_ctl.fail_if_busy } {
                return -EBUSY;
            }

            let ret = unsafe { ggtt_flush(vm) };
            if ret != 0 {
                return ret;
            }
            cond_resched();
            flags |= PIN_NONBLOCK as u32;
            continue;
        }

        // DRM-MM forbids allocator mutations during the scan; snapshot and
        // pin every selected VMA before unbinding any one of them.
        let mut vma: *mut I915Vma = ptr::null_mut();
        let mut next: *mut I915Vma = ptr::null_mut();
        list_for_each_entry_safe!(vma, next, &mut eviction_list, evict_link, {
            if unsafe { drm_mm_scan_remove_block(scan, ptr::addr_of_mut!((*vma).node)) } {
                unsafe { __i915_vma_pin(vma) };
            } else {
                unsafe {
                    list_del(ptr::addr_of_mut!((*vma).evict_link));
                    ungrab_vma(vma);
                }
            }
        });

        let mut ret = 0;
        let mut vma: *mut I915Vma = ptr::null_mut();
        let mut next: *mut I915Vma = ptr::null_mut();
        list_for_each_entry_safe!(vma, next, &mut eviction_list, evict_link, {
            unsafe { __i915_vma_unpin(vma) };
            if ret == 0 {
                ret = unsafe { __i915_vma_unbind(vma) };
            }
            unsafe { ungrab_vma(vma) };
        });

        while ret == 0 {
            let node = unsafe { drm_mm_scan_color_evict(scan) };
            if node.is_null() {
                break;
            }
            let vma = unsafe { container_of!(node, I915Vma, node) };
            if unsafe { (*node).color != I915_COLOR_UNEVICTABLE && grab_vma(vma, ww) } {
                ret = unsafe { __i915_vma_unbind(vma) };
                unsafe { ungrab_vma(vma) };
            } else {
                ret = -ENOSPC;
            }
        }
        return ret;
    }
}

// upstream: i915_gem_evict.c i915_gem_evict_for_node()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_evict_for_node(
    vm: *mut I915AddressSpace,
    ww: *mut I915GemWwCtx,
    target: *mut DrmMmNode,
    flags: u32,
) -> c_int {
    let mut eviction_list = ListHead {
        next: ptr::null_mut(),
        prev: ptr::null_mut(),
    };
    INIT_LIST_HEAD!(&mut eviction_list);
    let start_original = unsafe { (*target).start };
    let end_original = start_original.wrapping_add(unsafe { (*target).size });
    let mut start = start_original;
    let mut end = end_original;
    let mut ret = 0;

    lockdep_assert_held!(unsafe { &(*vm).mutex });
    GEM_BUG_ON!(start & (I915_GTT_PAGE_SIZE - 1) != 0);
    GEM_BUG_ON!(end & (I915_GTT_PAGE_SIZE - 1) != 0);
    unsafe { trace_i915_gem_evict_node(vm, target, flags) };

    if i915_is_ggtt(vm) {
        let ggtt = unsafe { i915_vm_to_ggtt(vm) };
        let mut gt: *mut IntelGt = ptr::null_mut();
        list_for_each_entry!(gt, ptr::addr_of_mut!((*ggtt).gt_list), ggtt_link, {
            unsafe { retire_gt_requests(gt) };
        });
    } else {
        unsafe { retire_gt_requests((*vm).gt) };
    }

    if unsafe { i915_vm_has_cache_coloring(vm) } {
        if start != 0 {
            start = start.wrapping_sub(I915_GTT_PAGE_SIZE);
        }
        end = end.wrapping_add(I915_GTT_PAGE_SIZE);
    }
    GEM_BUG_ON!(start >= end);

    let mut node = unsafe { __drm_mm_interval_first(&(*vm).mm, start, end.wrapping_sub(1)) };
    while !node.is_null() && unsafe { (*node).start < end } {
        if unsafe { (*node).color == I915_COLOR_UNEVICTABLE } {
            ret = -ENOSPC;
            break;
        }
        GEM_BUG_ON!(!unsafe { drm_mm_node_allocated(node) });
        let vma = unsafe { container_of!(node, I915Vma, node) };

        if unsafe { i915_vm_has_cache_coloring(vm) } {
            let node_end = unsafe { (*node).start.wrapping_add((*node).size) };
            if node_end == start_original && unsafe { (*node).color == (*target).color } {
                // The left neighboring object already shares the target color.
            } else if unsafe { (*node).start == end_original && (*node).color == (*target).color } {
                // The right neighboring object already shares the target color.
            } else {
                if unsafe { i915_vma_is_pinned(vma) } {
                    ret = -ENOSPC;
                    break;
                }
                if flags & PIN_NONBLOCK as u32 != 0 && unsafe { i915_vma_is_active(vma) } {
                    ret = -ENOSPC;
                    break;
                }
                if !unsafe { grab_vma(vma, ww) } {
                    ret = -ENOSPC;
                    break;
                }
                unsafe {
                    __i915_vma_pin(vma);
                    list_add(ptr::addr_of_mut!((*vma).evict_link), &mut eviction_list);
                }
            }
        } else {
            if unsafe { i915_vma_is_pinned(vma) } {
                ret = -ENOSPC;
                break;
            }
            if flags & PIN_NONBLOCK as u32 != 0 && unsafe { i915_vma_is_active(vma) } {
                ret = -ENOSPC;
                break;
            }
            if !unsafe { grab_vma(vma, ww) } {
                ret = -ENOSPC;
                break;
            }
            unsafe {
                __i915_vma_pin(vma);
                list_add(ptr::addr_of_mut!((*vma).evict_link), &mut eviction_list);
            }
        }

        let link = unsafe { ptr::addr_of_mut!((*node).node_list) };
        let next_link = unsafe { ptr::read_volatile(ptr::addr_of!((*link).next)) };
        node = unsafe { container_of!(next_link, DrmMmNode, node_list) };
    }

    let mut vma: *mut I915Vma = ptr::null_mut();
    let mut next: *mut I915Vma = ptr::null_mut();
    list_for_each_entry_safe!(vma, next, &mut eviction_list, evict_link, {
        unsafe { __i915_vma_unpin(vma) };
        if ret == 0 {
            ret = unsafe { __i915_vma_unbind(vma) };
        }
        unsafe { ungrab_vma(vma) };
    });
    ret
}

// upstream: i915_gem_evict.c i915_gem_evict_vm()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_evict_vm(
    vm: *mut I915AddressSpace,
    ww: *mut I915GemWwCtx,
    busy_bo: *mut *mut DrmI915GemObject,
) -> c_int {
    let mut ret = 0;
    lockdep_assert_held!(unsafe { &(*vm).mutex });
    unsafe { trace_i915_gem_evict_vm(vm) };

    if i915_is_ggtt(vm) {
        ret = unsafe { ggtt_flush(vm) };
        if ret != 0 {
            return ret;
        }
    }

    loop {
        let mut eviction_list = ListHead {
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
        };
        let mut locked_eviction_list = ListHead {
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
        };
        INIT_LIST_HEAD!(&mut eviction_list);
        INIT_LIST_HEAD!(&mut locked_eviction_list);

        let mut vma: *mut I915Vma = ptr::null_mut();
        list_for_each_entry!(vma, ptr::addr_of_mut!((*vm).bound_list), vm_link, {
            if unsafe { i915_vma_is_pinned(vma) } {
                continue;
            }

            let obj = unsafe { (*vma).obj };
            if unsafe { i915_gem_object_get_rcu(obj) }.is_null()
                || (!ww.is_null() && unsafe { object_locked_by_ww(obj, ww) })
            {
                unsafe {
                    __i915_vma_pin(vma);
                    list_add(
                        ptr::addr_of_mut!((*vma).evict_link),
                        &mut locked_eviction_list,
                    );
                }
                continue;
            }

            if !unsafe { i915_gem_object_trylock(obj, ww) } {
                if !busy_bo.is_null() {
                    unsafe { busy_bo.write(obj) };
                    ret = -EBUSY;
                    break;
                }
                unsafe { i915_gem_object_put(obj) };
                continue;
            }

            unsafe {
                __i915_vma_pin(vma);
                list_add(ptr::addr_of_mut!((*vma).evict_link), &mut eviction_list);
            }
        });

        if list_empty(&eviction_list) && list_empty(&locked_eviction_list) {
            break;
        }

        // Unbind locks already held by this caller's WW context first.
        let mut vma: *mut I915Vma = ptr::null_mut();
        let mut next: *mut I915Vma = ptr::null_mut();
        list_for_each_entry_safe!(vma, next, &mut locked_eviction_list, evict_link, {
            unsafe { __i915_vma_unpin(vma) };
            if ret == 0 {
                ret = unsafe { __i915_vma_unbind(vma) };
                if ret != -EINTR {
                    ret = 0;
                }
            }
            if !unsafe { dying_vma(vma) } {
                unsafe { i915_gem_object_put((*vma).obj) };
            }
        });

        // Then process newly locked objects and release their object locks.
        let mut vma: *mut I915Vma = ptr::null_mut();
        let mut next: *mut I915Vma = ptr::null_mut();
        list_for_each_entry_safe!(vma, next, &mut eviction_list, evict_link, {
            unsafe { __i915_vma_unpin(vma) };
            if ret == 0 {
                ret = unsafe { __i915_vma_unbind(vma) };
                if ret != -EINTR {
                    ret = 0;
                }
            }
            unsafe {
                i915_gem_object_unlock((*vma).obj);
                i915_gem_object_put((*vma).obj);
            }
        });

        if ret != 0 {
            break;
        }
    }
    ret
}
