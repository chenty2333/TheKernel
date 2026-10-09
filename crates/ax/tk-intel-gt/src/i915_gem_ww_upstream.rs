// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
//
//! Linux v7.2.3 `drivers/gpu/drm/i915/i915_gem_ww.c` translation and the
//! source-owned context layout from `i915_gem_ww.h`.
#![allow(unsafe_code)]

use core::{
    ffi::c_void,
    mem::{align_of, offset_of, size_of},
};

use crate::{
    i915_gem_object_api_upstream::{i915_gem_object_put, i915_gem_object_unlock},
    i915_gem_object_types_upstream::{DrmI915GemObject, intel_bo_to_drm_bo},
    intel_engine_cs_upstream::ListHead,
    linux::{
        gem::{dma_resv_lock_slow, dma_resv_lock_slow_interruptible},
        ww_mutex::{ww_acquire_fini, ww_acquire_init},
    },
    linux_list::{INIT_LIST_HEAD, list_add_tail, list_del_init},
};

/// `struct ww_acquire_ctx` storage for the selected x86_64 config:
/// RT/debug WW/lockdep disabled.
#[repr(C, align(8))]
pub struct WwAcquireCtx {
    pub task: *mut c_void,
    pub stamp: u64,
    pub acquired: u32,
    pub wounded: u16,
    pub is_wait_die: u16,
}

/// `struct i915_gem_ww_ctx` from the MIT `i915_gem_ww.h` owner header.
#[repr(C)]
pub struct I915GemWwCtx {
    pub ctx: WwAcquireCtx,
    pub obj_list: ListHead,
    pub contended: *mut DrmI915GemObject,
    pub intr: bool,
}

impl Default for I915GemWwCtx {
    fn default() -> Self {
        // A stack instance is immediately initialized by i915_gem_ww_ctx_init.
        // This C record contains only opaque bytes, integer state, and pointers.
        unsafe { core::mem::zeroed() }
    }
}

const _: [(); 24] = [(); size_of::<WwAcquireCtx>()];
const _: [(); 8] = [(); align_of::<WwAcquireCtx>()];
const _: [(); 56] = [(); size_of::<I915GemWwCtx>()];
const _: [(); 0] = [(); offset_of!(I915GemWwCtx, ctx)];
const _: [(); 24] = [(); offset_of!(I915GemWwCtx, obj_list)];
const _: [(); 40] = [(); offset_of!(I915GemWwCtx, contended)];
const _: [(); 48] = [(); offset_of!(I915GemWwCtx, intr)];

// upstream: i915_gem_ww.c i915_gem_ww_ctx_init()
pub unsafe fn i915_gem_ww_ctx_init(ww: *mut I915GemWwCtx, intr: bool) {
    unsafe { ww_acquire_init(core::ptr::addr_of_mut!((*ww).ctx)) };
    unsafe { INIT_LIST_HEAD(core::ptr::addr_of_mut!((*ww).obj_list)) };
    unsafe { (*ww).intr = intr };
    unsafe { (*ww).contended = core::ptr::null_mut() };
}

// upstream: i915_gem_ww.c i915_gem_ww_ctx_unlock_all()
unsafe fn i915_gem_ww_ctx_unlock_all(ww: *mut I915GemWwCtx) {
    loop {
        let obj = unsafe {
            list_first_entry_or_null!(
                core::ptr::addr_of_mut!((*ww).obj_list),
                DrmI915GemObject,
                obj_link,
            )
        };
        if obj.is_null() {
            break;
        }
        unsafe { list_del_init(core::ptr::addr_of_mut!((*obj).obj_link)) };
        unsafe { i915_gem_object_unlock(obj) };
        unsafe { i915_gem_object_put(obj) };
    }
}

// upstream: i915_gem_ww.c i915_gem_ww_unlock_single()
pub unsafe fn i915_gem_ww_unlock_single(obj: *mut DrmI915GemObject) {
    unsafe { list_del_init(core::ptr::addr_of_mut!((*obj).obj_link)) };
    unsafe { i915_gem_object_unlock(obj) };
    unsafe { i915_gem_object_put(obj) };
}

// upstream: i915_gem_ww.c i915_gem_ww_ctx_fini()
pub unsafe fn i915_gem_ww_ctx_fini(ww: *mut I915GemWwCtx) {
    unsafe { i915_gem_ww_ctx_unlock_all(ww) };
    WARN_ON!(unsafe { !(*ww).contended.is_null() });
    unsafe { ww_acquire_fini(core::ptr::addr_of_mut!((*ww).ctx)) };
}

// upstream: i915_gem_ww.c i915_gem_ww_ctx_backoff()
pub unsafe fn i915_gem_ww_ctx_backoff(ww: *mut I915GemWwCtx) -> i32 {
    let mut ret = 0;
    if WARN_ON!(unsafe { (*ww).contended.is_null() }) {
        return -crate::linux_config::EINVAL;
    }

    unsafe { i915_gem_ww_ctx_unlock_all(ww) };
    let contended = unsafe { (*ww).contended };
    let resv = unsafe { (*intel_bo_to_drm_bo(contended)).resv };
    if unsafe { (*ww).intr } {
        ret = unsafe { dma_resv_lock_slow_interruptible(resv, core::ptr::addr_of_mut!((*ww).ctx)) };
    } else {
        unsafe { dma_resv_lock_slow(resv, core::ptr::addr_of_mut!((*ww).ctx)) };
    }

    if ret == 0 {
        unsafe {
            list_add_tail(
                core::ptr::addr_of_mut!((*contended).obj_link),
                core::ptr::addr_of_mut!((*ww).obj_list),
            )
        };
    } else {
        unsafe { i915_gem_object_put(contended) };
    }
    unsafe { (*ww).contended = core::ptr::null_mut() };
    ret
}
