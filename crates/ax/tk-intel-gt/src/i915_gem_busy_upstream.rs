// SPDX-License-Identifier: MIT
// Copyright © 2014-2016 Intel Corporation.
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/gem/i915_gem_busy.c.

#![allow(dead_code, non_camel_case_types, non_snake_case, unsafe_code)]

use core::{
    ffi::{c_int, c_void},
    ptr,
};

use crate::{
    i915_gem_object_header_upstream::{DrmFileObjectLookup, i915_gem_object_lookup_rcu},
    i915_gem_object_types_upstream::{DrmI915GemObject, intel_bo_to_drm_bo},
    i915_request_types_upstream::I915Request,
    i915_request_upstream::{DmaFenceOps, i915_fence_ops, to_request},
    intel_context_upstream::DmaFence,
    intel_engine_types_upstream::IntelEngineCs,
    linux::{
        config::{DMA_RESV_USAGE_KERNEL, ENOENT},
        gem::{DmaResv, DrmGemObject},
        rcu::{rcu_read_lock, rcu_read_unlock},
        registers::I915_ENGINE_CLASS_INVALID,
        requests::{dma_fence_put, i915_request_completed},
    },
};

const I915_FENCE_FLAG_COMPOSITE: u32 =
    crate::i915_request_types_upstream::I915_FENCE_FLAG_COMPOSITE;
const DMA_RESV_USAGE_WRITE: u32 = DMA_RESV_USAGE_KERNEL + 1;
const DMA_RESV_USAGE_READ: u32 = DMA_RESV_USAGE_KERNEL + 2;

#[repr(C)]
struct DrmI915GemBusy {
    handle: u32,
    busy: u32,
}

#[repr(C)]
struct DmaFenceArrayView {
    base: DmaFence,
    num_fences: u32,
    _num_pending: i32,
    fences: *mut *mut DmaFence,
}

#[repr(C)]
struct DmaResvIter {
    obj: *mut DmaResv,
    usage: u32,
    fence: *mut DmaFence,
    fence_usage: u32,
    index: u32,
    fences: *mut c_void,
    num_fences: u32,
    is_restarted: bool,
}

unsafe extern "C" {
    static dma_fence_array_ops: DmaFenceOps;
    #[link_name = "dma_resv_iter_next"]
    fn busy_dma_resv_iter_next(cursor: *mut DmaResvIter) -> *mut DmaFence;
}

const _: [(); 48] = [(); core::mem::size_of::<DmaResvIter>()];

#[inline(always)]
// upstream: i915_gem_busy.c __busy_read_flag()
fn __busy_read_flag(id: u16) -> u32 {
    if id == I915_ENGINE_CLASS_INVALID as u16 {
        return 0xffff_0000;
    }
    assert!(id < 16, "GEM busy engine class exceeds uABI bitfield");
    0x1_0000u32 << id
}

#[inline(always)]
// upstream: i915_gem_busy.c __busy_write_id()
fn __busy_write_id(id: u16) -> u32 {
    // The uABI guarantees that an active writer is also in the read set. The
    // lockless lookup cannot establish that ordering, so report both bits.
    if id == I915_ENGINE_CLASS_INVALID as u16 {
        return u32::MAX;
    }
    (id as u32 + 1) | __busy_read_flag(id)
}

#[inline(always)]
// upstream: i915_gem_busy.c __busy_set_if_active()
unsafe fn __busy_set_if_active(fence: *mut DmaFence, flag: fn(u16) -> u32) -> u32 {
    if fence.is_null() {
        return 0;
    }
    if unsafe { (*fence).ops == ptr::addr_of!(dma_fence_array_ops).cast::<c_void>() } {
        let array = fence.cast::<DmaFenceArrayView>();
        let mut child = unsafe { (*array).fences };
        let mut nchild = unsafe { (*array).num_fences };
        while nchild != 0 {
            let current = unsafe { child.read() };
            child = unsafe { child.add(1) };
            nchild -= 1;
            // Composite arrays contain native i915 request fences only. A
            // foreign fence cannot be reported as busy by this ioctl.
            if current.is_null()
                || unsafe { (*current).ops != ptr::addr_of!(i915_fence_ops).cast::<c_void>() }
                || !crate::linux::bits::test_bit(I915_FENCE_FLAG_COMPOSITE, unsafe {
                    &(*current).flags
                })
            {
                return 0;
            }
            let request = unsafe { to_request(current) };
            if !i915_request_completed(unsafe { &*request }) {
                let engine: *mut IntelEngineCs = unsafe { (*request).engine };
                return flag(unsafe { (*engine).uabi_class });
            }
        }
        return 0;
    }

    if unsafe { (*fence).ops != ptr::addr_of!(i915_fence_ops).cast::<c_void>() } {
        return 0;
    }
    let request: *mut I915Request = unsafe { to_request(fence) };
    if i915_request_completed(unsafe { &*request }) {
        return 0;
    }
    let engine: *mut IntelEngineCs = unsafe { (*request).engine };
    flag(unsafe { (*engine).uabi_class })
}

#[inline(always)]
// upstream: i915_gem_busy.c busy_check_reader()
unsafe fn busy_check_reader(fence: *mut DmaFence) -> u32 {
    unsafe { __busy_set_if_active(fence, __busy_read_flag) }
}

#[inline(always)]
// upstream: i915_gem_busy.c busy_check_writer()
unsafe fn busy_check_writer(fence: *mut DmaFence) -> u32 {
    if fence.is_null() {
        return 0;
    }
    unsafe { __busy_set_if_active(fence, __busy_write_id) }
}

// upstream: i915_gem_busy.c i915_gem_busy_ioctl()
pub unsafe fn i915_gem_busy_ioctl(
    _dev: *mut c_void,
    data: *mut c_void,
    file: *mut crate::linux::gem::DrmFile,
) -> c_int {
    if data.is_null() || file.is_null() {
        return -crate::linux_config::EINVAL;
    }
    let args = data.cast::<DrmI915GemBusy>();
    let obj = unsafe {
        rcu_read_lock();
        i915_gem_object_lookup_rcu(file.cast::<DrmFileObjectLookup>(), (*args).handle)
    };
    if obj.is_null() {
        unsafe { rcu_read_unlock() };
        return -ENOENT;
    }

    unsafe { (*args).busy = 0 };
    let object_base: *mut DrmGemObject = unsafe { intel_bo_to_drm_bo(obj) };
    let reservation = unsafe { (*object_base).resv.cast::<DmaResv>() };
    let mut cursor: DmaResvIter = unsafe { core::mem::zeroed() };
    cursor.obj = reservation;
    cursor.usage = DMA_RESV_USAGE_READ;
    loop {
        let fence = unsafe { busy_dma_resv_iter_next(&mut cursor) };
        if fence.is_null() {
            break;
        }
        if cursor.is_restarted {
            unsafe { (*args).busy = 0 };
        }
        if cursor.fence_usage <= DMA_RESV_USAGE_WRITE {
            // Write fences contribute to both READ and WRITE engine bits.
            unsafe { (*args).busy |= busy_check_writer(fence) };
        } else {
            unsafe { (*args).busy |= busy_check_reader(fence) };
        }
    }
    unsafe {
        dma_fence_put(cursor.fence);
        rcu_read_unlock();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::{__busy_read_flag, __busy_write_id};
    use crate::linux::registers::I915_ENGINE_CLASS_INVALID;

    #[test]
    fn engine_busy_bits_match_i915_uabi() {
        assert_eq!(__busy_read_flag(0), 0x0001_0000);
        assert_eq!(__busy_read_flag(3), 0x0008_0000);
        assert_eq!(__busy_write_id(3), 0x0008_0004);
        assert_eq!(
            __busy_read_flag(I915_ENGINE_CLASS_INVALID as u16),
            0xffff_0000
        );
        assert_eq!(__busy_write_id(I915_ENGINE_CLASS_INVALID as u16), u32::MAX);
    }
}
