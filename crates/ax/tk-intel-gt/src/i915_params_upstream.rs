// SPDX-License-Identifier: MIT
// Copyright © 2018 Intel Corporation
// Source: Linux v7.2.3 drivers/gpu/drm/i915/i915_params.c (MIT).
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;

use crate::linux::i915_private::I915Params;

unsafe extern "C" {
    fn kfree(ptr: *mut c_void);
}

// upstream: i915_params.c _param_free_charp()
unsafe fn param_free_charp(valp: *mut *mut core::ffi::c_char) {
    unsafe {
        kfree((*valp).cast());
        *valp = core::ptr::null_mut();
    }
}

// upstream: i915_params.c i915_params_free()
// Frees the allocated members, not the params structure itself. The char *
// parameters are the ones named by I915_PARAMS_FOR_EACH (guc/huc/gsc firmware
// paths and force_probe); every other type is a no-op in `_param_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_params_free(params: *mut c_void) {
    let params = params.cast::<I915Params>();
    unsafe {
        param_free_charp(core::ptr::addr_of_mut!((*params).guc_firmware_path));
        param_free_charp(core::ptr::addr_of_mut!((*params).huc_firmware_path));
        param_free_charp(core::ptr::addr_of_mut!((*params).gsc_firmware_path));
        param_free_charp(core::ptr::addr_of_mut!((*params).force_probe));
    }
}
