// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B3e (CDCLK).
//! Replaced by intel_cdclk_upstream.rs (B3e).

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;

// upstream: intel_cdclk.c intel_cdclk_read_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cdclk_read_hw(_display: *mut IntelDisplay) {}

// upstream: intel_cdclk.c intel_init_cdclk_hooks()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_init_cdclk_hooks(_display: *mut IntelDisplay) {}

// upstream: intel_cdclk.c intel_cdclk_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cdclk_init(_display: *mut IntelDisplay) -> i32 {
    0
}

// upstream: intel_cdclk.c intel_update_max_cdclk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_update_max_cdclk(_display: *mut IntelDisplay) {}

// upstream: intel_cdclk.c intel_read_rawclk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_read_rawclk(_display: *mut IntelDisplay) -> u32 {
    24000
}

// upstream: intel_cdclk.c intel_cdclk_get_cdclk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cdclk_get_cdclk(
    _display: *mut IntelDisplay,
    _config: *mut core::ffi::c_void,
) {}

// upstream: intel_cdclk.c intel_cdclk_clock_changed()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cdclk_clock_changed(
    _a: *const core::ffi::c_void,
    _b: *const core::ffi::c_void,
) -> bool {
    false
}
