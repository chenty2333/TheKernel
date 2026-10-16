// SPDX-License-Identifier: MIT
// Copyright 2023, Intel Corporation.
// Source: drivers/gpu/drm/i915/i915_dsb_buffer.c
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ffi::c_void, mem::size_of, ptr};

use tk_drm_upstream::drm_device_types::DrmDevice;
use tk_linux_kapi::memory::{kfree, kzalloc_obj};

use crate::{
    i915_gem_core_upstream::i915_gem_object_ggtt_pin,
    i915_gem_internal_upstream::i915_gem_object_create_internal,
    i915_gem_lmem_upstream::i915_gem_object_create_lmem,
    i915_gem_object_api_upstream::i915_gem_object_put,
    i915_gem_object_header_upstream::i915_gem_object_flush_map,
    i915_gem_object_types_upstream::I915_MAP_WC,
    i915_gem_object_upstream::i915_gem_object_set_cache_coherency,
    i915_gem_pages_upstream::i915_gem_object_pin_map_unlocked,
    i915_vma_api_upstream::{I915_VMA_RELEASE_MAP, i915_vma_unpin_and_release},
    i915_vma_types_upstream::I915Vma,
    intel_parent_upstream::IntelDisplayDsbInterface,
    linux::{
        i915::{HAS_LMEM, i915_ggtt_offset, to_i915},
        registers::I915_CACHE_NONE,
    },
    linux_config::{ENOMEM, ERR_PTR, I915_BO_ALLOC_CONTIGUOUS, IS_ERR, PAGE_SIZE, PTR_ERR},
    linux_i915_private::DrmI915Private,
};

/// Source `struct intel_dsb_buffer` (private to i915_dsb_buffer.c; the display
/// side only sees it as an opaque pointer through `intel_display_dsb_interface`).
#[repr(C)]
struct IntelDsbBuffer {
    cmd_buf: *mut u32,
    vma: *mut I915Vma,
    buf_size: usize,
}

/// `PAGE_ALIGN(size)`.
#[inline]
const fn page_align(size: usize) -> usize {
    (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

// upstream: i915_dsb_buffer.c intel_dsb_buffer_ggtt_offset()
unsafe extern "C" fn intel_dsb_buffer_ggtt_offset(dsb_buf: *mut c_void) -> u32 {
    unsafe { i915_ggtt_offset((*dsb_buf.cast::<IntelDsbBuffer>()).vma) }
}

// upstream: i915_dsb_buffer.c intel_dsb_buffer_write()
unsafe extern "C" fn intel_dsb_buffer_write(dsb_buf: *mut c_void, idx: u32, val: u32) {
    unsafe {
        let dsb_buf = dsb_buf.cast::<IntelDsbBuffer>();
        *(*dsb_buf).cmd_buf.add(idx as usize) = val;
    }
}

// upstream: i915_dsb_buffer.c intel_dsb_buffer_read()
unsafe extern "C" fn intel_dsb_buffer_read(dsb_buf: *mut c_void, idx: u32) -> u32 {
    unsafe {
        let dsb_buf = dsb_buf.cast::<IntelDsbBuffer>();
        *(*dsb_buf).cmd_buf.add(idx as usize)
    }
}

// upstream: i915_dsb_buffer.c intel_dsb_buffer_fill()
unsafe extern "C" fn intel_dsb_buffer_fill(dsb_buf: *mut c_void, idx: u32, val: u32, size: usize) {
    unsafe {
        let dsb_buf = dsb_buf.cast::<IntelDsbBuffer>();
        // WARN_ON(idx > (dsb_buf->buf_size - size) / sizeof(*dsb_buf->cmd_buf));
        let limit = (*dsb_buf).buf_size.wrapping_sub(size) / size_of::<u32>();
        WARN_ON!(idx as usize > limit);

        // memset(&dsb_buf->cmd_buf[idx], val, size); memset() stores the low byte of `val`.
        ptr::write_bytes((*dsb_buf).cmd_buf.add(idx as usize).cast::<u8>(), val as u8, size);
    }
}

// upstream: i915_dsb_buffer.c intel_dsb_buffer_create()
unsafe extern "C" fn intel_dsb_buffer_create(drm: *mut DrmDevice, size: usize) -> *mut c_void {
    unsafe {
        let i915: *mut DrmI915Private = to_i915(drm.cast());
        let dsb_buf = kzalloc_obj::<IntelDsbBuffer>();
        if dsb_buf.is_null() {
            return ERR_PTR(-ENOMEM);
        }

        // Error exits of the C `goto err` path; `kfree(dsb_buf)` runs below.
        let ret: i32 = 'err: {
            let obj = if HAS_LMEM(i915) {
                let obj = i915_gem_object_create_lmem(
                    i915,
                    page_align(size) as u64,
                    I915_BO_ALLOC_CONTIGUOUS,
                );
                if IS_ERR(obj) {
                    break 'err PTR_ERR(obj);
                }
                obj
            } else {
                let obj = i915_gem_object_create_internal(i915, page_align(size) as u64);
                if IS_ERR(obj) {
                    break 'err PTR_ERR(obj);
                }
                i915_gem_object_set_cache_coherency(obj, I915_CACHE_NONE);
                obj
            };

            let mut vma = i915_gem_object_ggtt_pin(obj, ptr::null(), 0, 0, 0);
            if IS_ERR(vma) {
                let r = PTR_ERR(vma);
                i915_gem_object_put(obj);
                break 'err r;
            }

            let buf = i915_gem_object_pin_map_unlocked((*vma).obj, I915_MAP_WC);
            if IS_ERR(buf) {
                let r = PTR_ERR(buf);
                i915_vma_unpin_and_release(ptr::addr_of_mut!(vma), I915_VMA_RELEASE_MAP);
                break 'err r;
            }

            (*dsb_buf).vma = vma;
            (*dsb_buf).cmd_buf = buf.cast::<u32>();
            (*dsb_buf).buf_size = size;

            return dsb_buf.cast::<c_void>();
        };

        kfree(dsb_buf);
        ERR_PTR(ret)
    }
}

// upstream: i915_dsb_buffer.c intel_dsb_buffer_cleanup()
unsafe extern "C" fn intel_dsb_buffer_cleanup(dsb_buf: *mut c_void) {
    unsafe {
        let dsb_buf = dsb_buf.cast::<IntelDsbBuffer>();
        i915_vma_unpin_and_release(ptr::addr_of_mut!((*dsb_buf).vma), I915_VMA_RELEASE_MAP);
        kfree(dsb_buf);
    }
}

// upstream: i915_dsb_buffer.c intel_dsb_buffer_flush_map()
unsafe extern "C" fn intel_dsb_buffer_flush_map(dsb_buf: *mut c_void) {
    unsafe {
        let dsb_buf = dsb_buf.cast::<IntelDsbBuffer>();
        i915_gem_object_flush_map((*(*dsb_buf).vma).obj);
    }
}

/// i915-owned DSB buffer callbacks exported to the display parent.
#[unsafe(no_mangle)]
pub static i915_display_dsb_interface: IntelDisplayDsbInterface = IntelDisplayDsbInterface {
    ggtt_offset: Some(intel_dsb_buffer_ggtt_offset),
    write: Some(intel_dsb_buffer_write),
    read: Some(intel_dsb_buffer_read),
    fill: Some(intel_dsb_buffer_fill),
    create: Some(intel_dsb_buffer_create),
    cleanup: Some(intel_dsb_buffer_cleanup),
    flush_map: Some(intel_dsb_buffer_flush_map),
};
