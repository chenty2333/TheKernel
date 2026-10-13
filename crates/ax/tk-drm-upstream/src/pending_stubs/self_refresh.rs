// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Temporary stubs for drm_self_refresh_helper.c.

use core::ffi::c_void;

// upstream: drivers/gpu/drm/drm_self_refresh_helper.c drm_self_refresh_helper_alter_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_self_refresh_helper_alter_state(_state: *mut c_void) -> i32 {
    0
}

// upstream: drivers/gpu/drm/drm_self_refresh_helper.c drm_self_refresh_helper_update_avg_times()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_self_refresh_helper_update_avg_times(
    _state: *mut c_void,
    _commit_time_ms: u32,
    _new_self_refresh_mask: u32,
) {
}
