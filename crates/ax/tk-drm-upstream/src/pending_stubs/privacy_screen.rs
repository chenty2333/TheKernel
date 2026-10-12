// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drm_privacy_screen.c (no owner yet). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by drm_privacy_screen_upstream.rs when it lands.
//!
//! Symbols: drm_privacy_screen_get_state, drm_privacy_screen_set_sw_state.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;

// upstream: drm_privacy_screen.c (no owner yet): no privacy screen is attached here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_privacy_screen_get_state(
    _priv_: *mut c_void,
    sw_state: *mut u32,
    hw_state: *mut u32,
) {
    unsafe {
        *sw_state = 0;
        *hw_state = 0;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_privacy_screen_set_sw_state(_priv_: *mut c_void, _sw_state: u32) -> i32 {
    0
}
