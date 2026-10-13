// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Temporary stubs for drm_format_helper.c (format conversion functions).

use tk_linux_kapi::memory::kfree;
use crate::drm_format_helper_types::DrmFormatConvState;

// upstream: drivers/gpu/drm/drm_format_helper.c drm_format_conv_state_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_format_conv_state_init(state: *mut DrmFormatConvState) {
    if !state.is_null() {
        unsafe {
            (*state).tmp.mem = core::ptr::null_mut();
            (*state).tmp.size = 0;
            (*state).tmp.preallocated = false;
        }
    }
}

// upstream: drivers/gpu/drm/drm_format_helper.c drm_format_conv_state_copy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_format_conv_state_copy(
    state: *mut DrmFormatConvState,
    _old_state: *const DrmFormatConvState,
) {
    if !state.is_null() {
        unsafe {
            (*state).tmp.mem = core::ptr::null_mut();
            (*state).tmp.size = 0;
            (*state).tmp.preallocated = false;
        }
    }
}

// upstream: drivers/gpu/drm/drm_format_helper.c drm_format_conv_state_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_format_conv_state_release(state: *mut DrmFormatConvState) {
    if !state.is_null() {
        unsafe {
            if !(*state).tmp.preallocated && !(*state).tmp.mem.is_null() {
                kfree((*state).tmp.mem);
            }
            (*state).tmp.mem = core::ptr::null_mut();
            (*state).tmp.size = 0;
        }
    }
}
