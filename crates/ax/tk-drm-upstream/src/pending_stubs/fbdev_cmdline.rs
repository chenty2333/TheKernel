// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for drivers/video/cmdline.c video_get_options() (no owner yet). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by video_cmdline_upstream.rs when it lands.
//!
//! Symbols: video_get_options.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_char;

// upstream: video/cmdline.c (no owner yet): no per-connector video= option.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn video_get_options(_name: *const c_char) -> *const c_char {
    core::ptr::null()
}
