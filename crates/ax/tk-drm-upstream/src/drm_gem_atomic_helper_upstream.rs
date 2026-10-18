// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright 2026 TheKernel contributors.
//! Port of Linux `drivers/gpu/drm/drm_gem_atomic_helper.c` (batch B2b).

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;

use tk_linux_kapi::config::{EINVAL, ENOMEM, GFP_KERNEL};
use tk_linux_kapi::dma_fence_chain::dma_fence_chain_init;
use tk_linux_kapi::memory::{kfree, kzalloc};
use tk_linux_kapi::types::{DmaFence, DmaFenceChain};

use crate::dma_resv_types::DmaResv;
use crate::drm_atomic_state_helper_upstream::{
    __drm_atomic_helper_plane_destroy_state, __drm_atomic_helper_plane_duplicate_state,
    __drm_atomic_helper_plane_reset,
};
use crate::drm_gem_atomic_helper_types::{DrmShadowPlaneState, DrmSimpleDisplayPipe};
use crate::drm_gem_framebuffer_helper_upstream::{
    drm_gem_fb_get_obj, drm_gem_fb_vmap, drm_gem_fb_vunmap,
};
use crate::drm_plane_types::{DrmPlane, DrmPlaneState};

unsafe extern "C" {
    fn dma_fence_get(fence: *mut DmaFence) -> *mut DmaFence;
    fn dma_fence_put(fence: *mut DmaFence);
    fn dma_resv_get_singleton(resv: *mut DmaResv, usage: i32, fence: *mut *mut DmaFence) -> i32;
    fn dma_fence_chain_alloc() -> *mut DmaFenceChain;

    fn drm_format_conv_state_init(state: *mut crate::drm_format_helper_types::DrmFormatConvState);
    fn drm_format_conv_state_copy(
        state: *mut crate::drm_format_helper_types::DrmFormatConvState,
        old_state: *const crate::drm_format_helper_types::DrmFormatConvState,
    );
    fn drm_format_conv_state_release(
        state: *mut crate::drm_format_helper_types::DrmFormatConvState,
    );
}

pub const DMA_RESV_USAGE_KERNEL: i32 = 0;
pub const DMA_RESV_USAGE_WRITE: i32 = 1;

#[inline]
pub unsafe fn to_drm_shadow_plane_state(
    state: *mut DrmPlaneState,
) -> *mut DrmShadowPlaneState {
    state.cast::<DrmShadowPlaneState>()
}

// upstream: drm_gem_atomic_helper.c drm_gem_plane_helper_prepare_fb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_plane_helper_prepare_fb(
    _plane: *mut DrmPlane,
    state: *mut DrmPlaneState,
) -> i32 {
    unsafe {
        let mut fence = if !(*state).fence.is_null() {
            dma_fence_get((*state).fence)
        } else {
            core::ptr::null_mut()
        };

        if (*state).fb.is_null() {
            if !fence.is_null() {
                dma_fence_put(fence);
            }
            return 0;
        }

        let usage = if !fence.is_null() {
            DMA_RESV_USAGE_KERNEL
        } else {
            DMA_RESV_USAGE_WRITE
        };

        let num_planes = (*(*(*state).fb).format).num_planes as usize;
        let mut ret = 0;

        for i in 0..num_planes {
            let obj = drm_gem_fb_get_obj((*state).fb, i as u32);
            if obj.is_null() {
                ret = -EINVAL;
                break;
            }

            let mut new_fence: *mut DmaFence = core::ptr::null_mut();
            ret = dma_resv_get_singleton((*obj).resv, usage, &mut new_fence);
            if ret != 0 {
                break;
            }

            if !new_fence.is_null() && !fence.is_null() {
                let chain = dma_fence_chain_alloc();
                if chain.is_null() {
                    // Both references are owned here; the accumulator is
                    // released by the common error path below.
                    dma_fence_put(new_fence);
                    ret = -ENOMEM;
                    break;
                }
                dma_fence_chain_init(chain, fence, new_fence, 1);
                fence = &mut (*chain).base;
            } else if !new_fence.is_null() {
                fence = new_fence;
            }
        }

        if ret != 0 {
            if !fence.is_null() {
                dma_fence_put(fence);
            }
            return ret;
        }

        if !(*state).fence.is_null() {
            dma_fence_put((*state).fence);
        }
        (*state).fence = fence;
        0
    }
}

// upstream: drm_gem_atomic_helper.c __drm_gem_duplicate_shadow_plane_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_gem_duplicate_shadow_plane_state(
    plane: *mut DrmPlane,
    new_shadow_plane_state: *mut DrmShadowPlaneState,
) {
    unsafe {
        let plane_state = (*plane).state;
        let shadow_plane_state = to_drm_shadow_plane_state(plane_state);

        __drm_atomic_helper_plane_duplicate_state(plane, &mut (*new_shadow_plane_state).base);
        drm_format_conv_state_copy(
            &mut (*new_shadow_plane_state).fmtcnv_state,
            &(*shadow_plane_state).fmtcnv_state,
        );
    }
}

// upstream: drm_gem_atomic_helper.c drm_gem_duplicate_shadow_plane_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_duplicate_shadow_plane_state(
    plane: *mut DrmPlane,
) -> *mut DrmPlaneState {
    unsafe {
        let plane_state = (*plane).state;
        if plane_state.is_null() {
            return core::ptr::null_mut();
        }

        let new_shadow_plane_state = kzalloc(
            core::mem::size_of::<DrmShadowPlaneState>(),
            GFP_KERNEL,
        )
        .cast::<DrmShadowPlaneState>();
        if new_shadow_plane_state.is_null() {
            return core::ptr::null_mut();
        }

        __drm_gem_duplicate_shadow_plane_state(plane, new_shadow_plane_state);
        &mut (*new_shadow_plane_state).base
    }
}

// upstream: drm_gem_atomic_helper.c __drm_gem_destroy_shadow_plane_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_gem_destroy_shadow_plane_state(
    shadow_plane_state: *mut DrmShadowPlaneState,
) {
    unsafe {
        drm_format_conv_state_release(&mut (*shadow_plane_state).fmtcnv_state);
        __drm_atomic_helper_plane_destroy_state(&mut (*shadow_plane_state).base);
    }
}

// upstream: drm_gem_atomic_helper.c drm_gem_destroy_shadow_plane_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_destroy_shadow_plane_state(
    _plane: *mut DrmPlane,
    plane_state: *mut DrmPlaneState,
) {
    unsafe {
        let shadow_plane_state = to_drm_shadow_plane_state(plane_state);
        __drm_gem_destroy_shadow_plane_state(shadow_plane_state);
        kfree(shadow_plane_state.cast::<c_void>());
    }
}

// upstream: drm_gem_atomic_helper.c __drm_gem_reset_shadow_plane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_gem_reset_shadow_plane(
    plane: *mut DrmPlane,
    shadow_plane_state: *mut DrmShadowPlaneState,
) {
    unsafe {
        if !shadow_plane_state.is_null() {
            __drm_atomic_helper_plane_reset(plane, &mut (*shadow_plane_state).base);
            drm_format_conv_state_init(&mut (*shadow_plane_state).fmtcnv_state);
        } else {
            __drm_atomic_helper_plane_reset(plane, core::ptr::null_mut());
        }
    }
}

// upstream: drm_gem_atomic_helper.c drm_gem_reset_shadow_plane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_reset_shadow_plane(plane: *mut DrmPlane) {
    unsafe {
        if !(*plane).state.is_null() {
            drm_gem_destroy_shadow_plane_state(plane, (*plane).state);
            (*plane).state = core::ptr::null_mut();
        }

        let shadow_plane_state = kzalloc(
            core::mem::size_of::<DrmShadowPlaneState>(),
            GFP_KERNEL,
        )
        .cast::<DrmShadowPlaneState>();
        __drm_gem_reset_shadow_plane(plane, shadow_plane_state);
    }
}

// upstream: drm_gem_atomic_helper.c drm_gem_begin_shadow_fb_access()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_begin_shadow_fb_access(
    _plane: *mut DrmPlane,
    plane_state: *mut DrmPlaneState,
) -> i32 {
    unsafe {
        let shadow_plane_state = to_drm_shadow_plane_state(plane_state);
        let fb = (*plane_state).fb;
        if fb.is_null() {
            return 0;
        }

        drm_gem_fb_vmap(
            fb,
            (*shadow_plane_state).map.as_mut_ptr(),
            (*shadow_plane_state).data.as_mut_ptr(),
        )
    }
}

// upstream: drm_gem_atomic_helper.c drm_gem_end_shadow_fb_access()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_end_shadow_fb_access(
    _plane: *mut DrmPlane,
    plane_state: *mut DrmPlaneState,
) {
    unsafe {
        let shadow_plane_state = to_drm_shadow_plane_state(plane_state);
        let fb = (*plane_state).fb;
        if fb.is_null() {
            return;
        }

        drm_gem_fb_vunmap(fb, (*shadow_plane_state).map.as_mut_ptr());
    }
}

// upstream: drm_gem_atomic_helper.c drm_gem_simple_kms_begin_shadow_fb_access()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_simple_kms_begin_shadow_fb_access(
    pipe: *mut DrmSimpleDisplayPipe,
    plane_state: *mut DrmPlaneState,
) -> i32 {
    unsafe { drm_gem_begin_shadow_fb_access(&mut (*pipe).plane, plane_state) }
}

// upstream: drm_gem_atomic_helper.c drm_gem_simple_kms_end_shadow_fb_access()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_simple_kms_end_shadow_fb_access(
    pipe: *mut DrmSimpleDisplayPipe,
    plane_state: *mut DrmPlaneState,
) {
    unsafe { drm_gem_end_shadow_fb_access(&mut (*pipe).plane, plane_state); }
}

// upstream: drm_gem_atomic_helper.c drm_gem_simple_kms_reset_shadow_plane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_simple_kms_reset_shadow_plane(
    pipe: *mut DrmSimpleDisplayPipe,
) {
    unsafe { drm_gem_reset_shadow_plane(&mut (*pipe).plane); }
}

// upstream: drm_gem_atomic_helper.c drm_gem_simple_kms_duplicate_shadow_plane_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_simple_kms_duplicate_shadow_plane_state(
    pipe: *mut DrmSimpleDisplayPipe,
) -> *mut DrmPlaneState {
    unsafe { drm_gem_duplicate_shadow_plane_state(&mut (*pipe).plane) }
}

// upstream: drm_gem_atomic_helper.c drm_gem_simple_kms_destroy_shadow_plane_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_simple_kms_destroy_shadow_plane_state(
    pipe: *mut DrmSimpleDisplayPipe,
    plane_state: *mut DrmPlaneState,
) {
    unsafe { drm_gem_destroy_shadow_plane_state(&mut (*pipe).plane, plane_state); }
}
